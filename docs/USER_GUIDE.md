# Hermit User Guide

Hermit runs Linux programs in a controlled environment and replaces or
sanitizes sources of nondeterminism such as time, random data, file metadata,
and thread scheduling. Its main uses are reproducible execution, controlled
concurrency testing, and experimental record/replay debugging.

Hermit is not a security boundary. The guest can see most of the host file
system by default, and changes to file contents can change a run. Complete
reproducibility still requires fixed input files and no external network
dependency.

Compatibility is substantial but incomplete, especially for less common Linux
system calls.

Chosen IDs for `--no-namespace` have a reserved-input admission contract,
including named refusals for legacy Chosen schedule JSON and raw Chosen binary
record/replay. See [Chosen process IDs](CHOSEN_IDS.md) for the supported scope,
native-rseq incompatibility, observable mapping and remaining boundaries.

## Supported Environment

Hermit supports x86_64 Linux. It requires:

- A Rust nightly toolchain. The repository's `rust-toolchain.toml` selects it
  automatically when Rust is managed by `rustup`.
- Linux user, PID, IPC, and mount namespaces.
- Parent-child `ptrace` and seccomp filter support.
- `libunwind` and LZMA development libraries.
- User-space performance counters for precise thread preemption. Hermit can
  run without them, but it disables timer preemption and prints a warning.

On Debian or Ubuntu:

```bash
sudo apt-get update
sudo apt-get install -y build-essential pkg-config libunwind-dev liblzma-dev
```

For the registry release, install stable Rust with rustup, then use
`cargo +stable install hermit-run --version 0.4.1 --locked`. The package's
executable is named `hermit`. The 0.4.1 ptrace release is qualified on Ubuntu
24.04's stock Linux 6.8 kernel. Source builds continue to use the toolchain
selected by `rust-toolchain.toml`.

Ubuntu 24.04 may deny unprivileged user namespaces through AppArmor even when
`kernel.unprivileged_userns_clone=1`. Errors include `MapUid: EPERM` and
`Setting UID map failed`. An administrator can allow the installed Hermit
executable through a scoped profile while keeping
`kernel.apparmor_restrict_unprivileged_userns=1`. Find its absolute path with
`readlink -f "$(command -v hermit)"`; create `/etc/apparmor.d/hermit-local`,
substituting that path for the example attachment below:

```text
abi <abi/4.0>,
include <tunables/global>

profile hermit /home/YOUR_USER/.cargo/bin/hermit flags=(unconfined) {
  userns,
}
```

Load it with `sudo apparmor_parser -r /etc/apparmor.d/hermit-local`, then retry
`hermit run -- /bin/echo hello`. This grants user-namespace access to the named
executable; review that path and local security policy before applying the
profile, and update it if the installation path changes.

On Fedora or CentOS:

```bash
sudo dnf install -y libunwind-devel xz-devel
```

Hermit may work in a VM or container if the host exposes the required kernel
facilities. Nested containers commonly block namespaces, `ptrace`, seccomp, or
`perf_event_open`; see [Troubleshooting](#troubleshooting).

## Getting Started

### Build And Install

Clone the repository with its submodules:

```bash
git clone --recurse-submodules https://github.com/rrnewton/hermit
cd hermit
```

If you already cloned it, initialize the submodules from the repository root:

```bash
git submodule update --init --recursive
```

Then build the workspace:

```bash
cargo build --workspace
./target/debug/hermit --version
```

The debug executable is `target/debug/hermit`. For an optimized build:

```bash
cargo build --release --workspace
./target/install_pkg/hermit --version
```

A regular build does not embed the Git revision, so `hermit --version` reports
`hermit 0.4.0 (2026-10-02, source revision not embedded)` and `hermit version --json` reports
`"git_sha": "unknown"`. This keeps a commit that does not change Hermit's
source from rebuilding it. To stamp the revision, as a release build does, set
`HERMIT_STAMP_GIT_SHA=1` when building; the version then reads
`hermit 0.4.0 (2026-10-02, g0123456789ab)`, with a `-dirty` suffix when tracked
files differed from that commit. `make release-core` sets it for you.

The release build creates `target/install_pkg/hermit` and
`target/install_pkg/rsrcs/`. The resource directory contains the SaBRe and
e9patch rewriters, Detcore backend shared libraries, and the minimal DynamoRIO
runtime. Hermit discovers it beside the invoked executable and, for an in-tree
release binary, under `target/install_pkg`. Set
`HERMIT_INSTALL_DIR=/path/to/install` to select another installation root.
Backend-specific environment variables remain compatibility overrides, but a
complete package needs none of them. The default CLI does not enable `dbt`,
`sabre`, or `e9patch` just because these resources are present; see the
[feature-enabled build](#backend-selection) below.

`target/install_pkg/hermit` is a staging symlink to the release binary. Follow
it when creating a standalone installation, for example:

```bash
cp -aL target/install_pkg/ /opt/hermit
/opt/hermit/hermit --version
```

To install the current checkout into Cargo's binary directory, normally
`~/.cargo/bin`:

```bash
cargo install --locked --path hermit-cli
hermit --version
```

All examples below assume `hermit` is on `PATH`. Substitute
`./target/debug/hermit` when using an uninstalled debug build.

### First Run

Run a command by placing `hermit run` before it:

```bash
hermit run -- /bin/echo "hello from Hermit"
```

The `--` separator is recommended. It makes every following argument part of
the guest command, even when a guest argument begins with `-`.

For a quick determinism check, run the same virtual random-data read twice:

```bash
hermit run -- /bin/sh -c 'od -An -N8 -tx1 /dev/urandom'
hermit run -- /bin/sh -c 'od -An -N8 -tx1 /dev/urandom'
```

Both invocations should print the same bytes when the command, inputs, and
Hermit configuration are unchanged.

Hermit uses strict deterministic execution by default. The explicit
`--strict` option also requires every input of the run on the command line: a
run without `--epoch` (or `HERMIT_EPOCH`, or a replayed recording that supplies
the epoch), or with `--seed-from=SystemRandom`, is refused before the guest
starts instead of drawing that input from the host.

A run on a backend with precise timer events (ptrace, e9patch preprocessing on
it, or KVM) whose precise timers preempted a thread late (a skid overshoot: the
counter's interrupt arrived past its target) is refused at the end with exit
status 122, since the moved preemption point is not deterministic evidence.
`--summary` and `--summary-json` record the number of skid-overshoot events
(`skid_overshoots`, with `seed_reproducible: false`). Reverie and Detcore can
each report one overshoot, so an event is one thread's late timer and its
target, however many reports name it; the JSON also records the reports
(`skid_overshoot_reports`), which the refusal counts. Both describe that
execution, not its seed: the same seed can run clean once and overshoot the
next time, so a seed is never classified. An absent `seed_reproducible` with
`skid_overshoots: 0` means that no overshoot was observed in that run, not that
the seed reproduces it.

## Choosing A Mode

| Goal | Command |
| --- | --- |
| Run deterministically | `hermit run -- PROGRAM ARGS...` |
| Explore thread schedules reproducibly | `hermit run --chaos --sched-seed=N -- PROGRAM` |
| Compare two deterministic executions | `hermit run --verify -- PROGRAM` |
| Record one execution | `hermit record start -- PROGRAM` |
| Replay a recording without GDB | `hermit replay --autopilot [ID]` |
| Diagnose a failing chaos execution | `hermit analyze ... -- PROGRAM` |

### Deterministic Run

`hermit run` is the normal mode. By default it:

- serializes guest threads and schedules them deterministically;
- makes I/O completion behavior deterministic;
- virtualizes time, random inputs, CPUID, and selected file metadata;
- rejects syscalls classified Unsupported instead of forwarding them silently;
- gives the guest isolated PID and IPC namespaces, `/tmp`, `/dev/shm` and
  `/dev/mqueue`;
- uses an isolated local network namespace;
- exposes most of the host file system read/write.

For a cleaner starting environment, use a minimal environment and pass only
the variables the program needs:

```bash
hermit run --base-env=minimal -e LANG=C --workdir=/tmp -- /bin/pwd
```

#### Backend Selection

Use `--backend=ptrace|liteinst|in-guest-trap|kvm` to select the core process
instrumentation backend. It is a global option and belongs before the
subcommand, but backend scope is command-specific. KVM also supports `analyze`
and `bisect`, which launch `run` trials (`hermit --backend=kvm analyze ...`);
unsupported combinations fail closed. The default is `ptrace`, so existing
commands are unchanged:

```bash
hermit --backend=ptrace run -- /bin/echo hello
```

Hermit detects whether the requested backend is integrated and available on
the current host. It does not silently fall back to a different backend.
LiteInst runs Detcore inside the guest. Hermit preloads
`libdetcore_liteinst.so`: the one `HERMIT_LITEINST_TOOL_RUNTIME` names, else
the installation's `rsrcs/libdetcore_liteinst.so` (which `hermit-install`
stages), else the one built beside the Hermit executable. Its constructor installs Detcore's `Tool` in
the guest process before the program's `main`, and Detcore's global state stays
in Hermit, reached over a socket. No ptrace tracer sits on the system-call path.

Build the runtime beside a Hermit built with the optional `liteinst` feature;
a build without it refuses `--backend=liteinst`:

```bash
cargo build --locked -p hermit-run --features liteinst --bin hermit
cargo build --locked -p detcore-liteinst
./target/debug/hermit --backend=liteinst run --max-timeslice=disabled -- /bin/echo hello
```

Hermit verifies the library's architecture and that it registers
`detcore_liteinst_initialize` as a preload constructor before it starts the
guest; an arbitrary shared object or a constructor-free library is rejected
rather than letting the guest run unmonitored. The dynamic loader, the C
library's initialization and the constructors of the program's own shared
libraries run before that constructor and are not monitored.

Detcore's constructor uses a guarded 8 MiB bootstrap stack in the fixed Tool
region `[0x600000000000, 0x600100000000)`. That allocation remains reserved
after initialization and is disjoint from the runtime's signal and callback
stacks, including their guard pages. A conflicting mapping refuses startup.
This provides stack placement and boundary guards; guest writes to writable
Tool interiors, constructor environment changes, and shared libc/TLS state
remain outside the isolation guarantee. The standalone Reverie preload's
constructor still uses its caller's stack.

The in-guest runtime cannot deliver Detcore's preemption timer yet, so Hermit
refuses a LiteInst run unless `--max-timeslice=disabled` is given. It also
refuses `--verify`, because the in-guest Tool does not forward its records to
Hermit yet, and `--timeout`, `--skid-margin` and `--gdbserver`. It refuses guest programs it cannot monitor: statically linked,
set-user-ID or non-x86-64 programs, programs with a `DT_PREINIT_ARRAY`, and
anything other than a regular file. A guest may `fork`, and `vfork` runs as a
copying fork; creating a thread and `exec` are not supported yet.

A guest process leaves the host some time after Hermit grants its exit, so
Hermit selects no other guest's turn until the process has physically gone.
An exit that needs another guest's turn to finish therefore cannot complete.
A guest that serves a kernel network-filesystem client mounted outside the
guest set (NFS, CIFS, 9p) is an unsupported topology. Hermit fails the run,
naming the process, when an exit sleeps in the kernel's exit path for over
1 s, or when any exit is not complete 60 s after its grant, and kills the
guests; such a run never verifies. A soft mount whose retries give up in
under a second can still complete unnoticed.
For the same reason Hermit refuses, by name, a guest that holds a FUSE or
CUSE device (`/dev/fuse`, `/dev/cuse`) or a seccomp user-notification
listener, whether it opens one, starts with one, or receives one through
`SCM_RIGHTS` or `pidfd_getfd`: such a guest could be the server another
guest's exit waits for. Reading plain files on a FUSE filesystem is not
affected, and `userfaultfd` is supported.

In-guest LiteInst needs Linux 6.5 or later (`SO_PEERPIDFD`) and refuses to
start on an older kernel. `--verify` refuses to compare a run that recorded a
determinism loss (an in-guest process died at a moment the host chose,
without finishing its exit, or a DETLOG record was lost) or whose log failed
a write, even if later writes succeeded. The runtime schedules a process's
own `kill`, `tkill` or `tgkill` (including `abort` and `raise`) when it names
the calling process or thread and sends an unblocked standard signal with a
terminating default action. This includes the guest's default SIGTRAP action,
but excludes SIGSYS and SIGSEGV when the runtime owns its handler. It also
excludes real-time signals, process-group sends and signal deaths after a
guest seccomp filter is admitted; filter installation may itself be refused
by the runtime's existing admission policy. These excluded deaths retain
the deregistration-loss check and cannot gain verification credit.

For a supported self-signal, the runtime finishes its exit in its own turn
before the kernel kills it. Verification can then compare the run if the
program's result otherwise qualifies; scheduling an abort does not turn a
failing program into a passing one. Because that exit is reported before
the death, the status Detcore
receives names the signal exactly, but whether a core was dumped is only
predicted (from the signal, the process's dumpable mode, `RLIMIT_CORE` and
`core_pattern`) and differs when the kernel fails to write the core; Detcore
does not use it, and the guest's parent sees the kernel's own status. A
death outside the supported delivery paths records the loss, including a
hardware fault, a signal another process sends, and a SIGPIPE from a write.

Both in-guest backends also schedule a default-fatal SIGALRM from a one-shot
virtual `alarm` or `ITIMER_REAL` when the single-thread guest is parked in an
emulated `pause` or `nanosleep` and does not block SIGALRM. No guest-handler
opt-in is needed. The runtime reports the signal and exit callbacks before
the kernel terminates the process; a fatal sleep does not copy a remaining
time to the guest. Ignored or blocked SIGALRM does not interrupt these sleeps,
and a blocked alarm remains pending after canceling its timer. This bridge
does not cover recurring timers, other timer kinds, external signals, a
guest running outside these sleeps, physical or polling I/O, or a sleep
already at its deadline. Those deaths retain the loss check. The existing
no-guest-filter launch and callback contract still applies.

On a run qualified for guest SIGALRM handlers, `in-guest-trap` with the
per-run opt-in `REVERIE_LITEINST_SIGALRM_HANDLERS=1` permits `getuid`,
`geteuid`, `getgid` and `getegid` while the handler is installed, including
inside the handler. They return Hermit's existing virtual-root identity
(`0`). Handler admission still requires site patching to be off; requesting
the opt-in on patched LiteInst does not admit a handler. Buffered credential
queries, `getppid` and the other unplaced calls remain refused with
`EOPNOTSUPP` while a handler is installed, as does closing inherited stderr.
Handler admission remains disabled unless explicitly requested.

The default namespace, mount, and network setup is shared with Hermit's other
backends; `--no-namespace` remains available for trusted guests. The in-guest
runtime reserves `SIGSYS` for itself. This experimental path is not a security
boundary for intentionally hostile code.

DynamoRIO's static frontend and runtime resources are built and staged by
[hermit-dynamorio](https://github.com/rrnewton/hermit-dynamorio). SaBRe and
e9patch are built and staged by
[hermit-3rdparty-patching](https://github.com/rrnewton/hermit-3rdparty-patching).
Follow those repositories' maintained frontend, artifact, and validation
instructions. Their backend selections are unavailable in the core executable;
a core build never substitutes ptrace for an explicit unavailable selection.

For the external DynamoRIO frontend, the default `--network=local` gives each
physical execution a fresh network namespace with active loopback, including both executions of
`--verify`. Each guest and its verification channel share that namespace.
Newly created guest sockets are isolated from unrelated outer listeners; a
namespace setup failure stops before the guest runs. Explicit `--network=host`
shares the caller's network and remains incompatible with `--strict`.

SaBRe and e9patch use their patching repository's static frontends. Core KVM
requires read-write `/dev/kvm` access plus a guest-kernel ABI, and an unlimited
address-space limit: it reserves 3 GiB of host address space for every guest
process, plus 3 GiB for each exec in flight, so Hermit refuses to start it under a finite
`RLIMIT_AS` (`kvm_address_space_limit`, exit 122; see
[ERROR_CATALOG.md](ERROR_CATALOG.md)). Bound its memory with a cgroup instead.

On KVM, Detcore decides a guest's `flock(2)` locks inside the run and does not
place them on the host: a guest's lock does not exclude a process outside the
run, such as a writer on the host or another Hermit run, and theirs do not
exclude the guest. Files that only the run uses are not affected. Run a guest
that shares flock-protected files with processes outside the run, such as a
`cargo` home or a `~/.rustup` that a host process also uses, on the ptrace
backend, which places each of the run's locks on the host
(https://github.com/rrnewton/hermit/issues/5043).

See [SaBRe backend compatibility](SABRE_COMPATIBILITY.md) for the measured
`Stripped` allowlist, build commands, and known gaps. An enabled probe is
not a blanket support claim for every workload in its subsystem.

`e9patch` is an experimental hybrid rather than a standalone Detcore runtime.
It uses the cached offline instruction map and conservative `e9tool -O0` mode
to apply `before empty` trampolines at exact candidate offsets in the main ELF.
Because the linear candidate scan
can include embedded data, e9tool decides which candidates are instructions;
Hermit reports both counts and rejects partial coverage of the recovered set.
Hermit does not enable e9patch's B0 fallback because reserving SIGILL would
change guest signal semantics. The rewritten program is bind-mounted read-only
at the original executable path, then runs under the ptrace Detcore backend, which
preserves strict-mode semantics for trapped events in shared libraries, the
vDSO, and dynamic code. Empty trampolines do not make raw `RDRAND`, `RDSEED`,
or TSX deterministic even when those sites are mapped, so those instructions
remain unsupported. Privilege-bearing executables fail closed rather than
losing set-ID or file-capability semantics. This first integration validates
rewrite coverage; it does not yet remove ptrace overhead. The workspace
installation package includes `rsrcs/e9tool`. For a custom installation, put
`e9tool` in `PATH` or set `HERMIT_E9TOOL=/path/to/e9tool`.
Non-ELF entrypoints, including shebang scripts, skip preprocessing and run
through the ptrace correctness path.

`--namespace-only` bypasses instrumentation entirely. Combining it with any
explicit `--backend` selection is rejected because the backend would be ignored.

Hermit does not snapshot the host file system. If `PROGRAM` reads a file that
changes between runs, the result is allowed to change. Use immutable inputs,
a fixed container image, or explicit mounts to control this dependency.

### Chaos Mode

Chaos mode varies deterministic scheduling decisions to expose concurrency
bugs while keeping each selected execution reproducible:

```bash
hermit run --chaos --sched-seed=7 -- ./target/debug/hello_race
```

Run the same command with the same seed to reproduce that schedule. Try other
seeds to explore other schedules:

```bash
for seed in 1 2 3 4 5; do
  hermit run --chaos --sched-seed="$seed" -- ./target/debug/hello_race
done
```

On a host whose retired-branch counter fails Reverie's performance-counter
validation (`hermit host-capabilities` reports `exact-branch-counter: ABSENT`),
the same seed is not guaranteed to reproduce a schedule when the run has a
timeslice, because preemptions are timed with that counter. On the ptrace and
e9patch backends `--strict` refuses such a run; without it, `hermit run`,
`hermit analyze`, `hermit bisect` and `hermit record start` go on but first
print one line that starts `WARNING: this host's retired-branch counter failed`.
Rerun a finding from such a host on one where `hermit host-capabilities`
reports `exact-branch-counter: PRESENT` with the validation passed before
treating it as reproducible.

The seed options have distinct roles:

- `--sched-seed=N` controls scheduler randomness.
- `--rng-seed=N` controls virtual random data supplied to the guest.
- `--seed=N` is the fallback for both when a specific seed is not supplied.
- `--seed-from=SystemRandom` chooses and prints a fresh seed. Record the printed
  value to reproduce the run.
- `--seed-from=Args` derives a stable seed from the guest command and arguments.

Chaos scheduling is most effective when hardware performance counters are
available. `--max-timeslice=N` controls the longest uninterrupted virtual
time slice. Smaller values create more scheduling opportunities at additional
runtime cost; positive values must be at least one RCB, which is 10 virtual
nanoseconds at the default clock multiplier and scales with that multiplier.
The cost grows steeply: each slice of a thread that computes without a syscall
ends with a timer event, which the ptrace and KVM backends place by stepping
the guest through up to the processor's skid margin (100 to 10000 RCBs, or
`--skid-margin`), so a CPU-bound guest can run thousands of times slower at
`--max-timeslice=1000` than at the default. When a slice is at most ten skid
margins long (on a processor with a 10000-RCB margin, `--max-timeslice` of
1000000 or less at the default clock multiplier), `hermit run`, `hermit
analyze` and `hermit bisect` print a `NOTICE:` line saying so; there, a run that
`--timeout` cuts off may be slow rather than hung, and `--summary` reports how
many timeslices it had.
`--target-timeslice=N` adds a cheaper logical deadline checked at syscall
boundaries; it is useful for workloads that enter the kernel frequently.
`--max-timeslice=disabled` avoids PMU use but can miss bugs in
CPU-bound code that rarely makes system calls. The old
`--preemption-timeout` spelling is a deprecated alias for
`--max-timeslice`.

The value that finds a race depends on the race. Chaos ends each timeslice after
a random number of retired conditional branches, drawn from an exponential
distribution whose mean parameter is the maximum timeslice and capped at the
maximum, so slices average about 0.63 of it before rounding to whole branches. A
race whose window spans a few branches needs a maximum of about a hundred
branches (`--max-timeslice=1000`); a race whose other thread must first run far
into its own work needs slices long enough to get there, and short slices miss
it. No single value finds both. Rather than guessing, let each seed draw its own
value:

```bash
for seed in $(seq 0 63); do
  hermit run --chaos --chaos-seeded-timeslice --sched-seed="$seed" -- ./target/debug/hello_race
done
```

Two in five seeds draw their maximum from 1000 to 2000 virtual nanoseconds (100
to 200 branches, for windows of a few dozen branches), two in five from 1000000
to 200000000 (for races whose other thread must first run far), and one in five
from the decades between, each log-uniformly within its range. Where
`--clock-multiplier` makes one RCB longer than the value drawn, the maximum is
one RCB, and a replay of the run under the same clock multiplier and slowdown
settings keeps it. The races measured so far needed one end or the other: a
46-branch window in a multithreaded C++ test binary failed in 9 of 32 seeds at
1000 and in none of 32 at 1e4, 1e5, 1e6 or the default. Hermit prints the value:
`[hermit] --chaos-seeded-timeslice: scheduler seed 7 drew --max-timeslice=...`.
Rerunning the seed reproduces the run, and so does passing the printed value as
`--max-timeslice` instead of the flag. The draw is part of what a seed selects,
so a Hermit release that changed it would change every seed's schedule; the
printed value pins it. Seeds that draw small values run much slower: every
timeslice ends with a timer event, and the ptrace backend steps the guest
through the last skid margin (100 to 10000 branches, depending on the processor)
of every slice. The option needs the PMU timer of the ptrace backend; on other
backends, set chaos's cooperative deadline with `--target-timeslice`.

Every scheduler turn adds a fixed charge to the guest-visible clock
(`--scheduler-turn-cost`, 500000 virtual nanoseconds by default), including
the turn that ends a timeslice. With slices far shorter than the charge,
CPU-bound work therefore appears about `1 + charge / slice` times slower to
the guest: 501 times at `--max-timeslice=1000`, so a test's own timeouts can
expire before the code under test runs. `--cap-preemption-charge` charges a
turn that ends an expired slice at most the virtual time its thread ran since
its previous turn, which bounds that factor at 2. A thread's previous turn can
be a syscall inside the same slice: a preemption whose thread ran at least one
charge since its previous turn is charged as before, and one that follows a
syscall by less is charged only the time since that syscall, however long the
slice was. A capped turn whose full charge is nonzero still adds at least
one virtual nanosecond; a full charge that `--clock-multiplier` makes zero
stays zero. A recorded schedule stores the setting, and a replay under the
other is refused.

The option changes what the guest observes. After a capped preemption,
`clock_gettime(CLOCK_MONOTONIC)`, and every other clock Hermit derives from
the same virtual time, has advanced less than it would without the option. A
timer or timeout measured on those clocks (a sleep, a futex, `poll` or
`epoll_wait` timeout, a POSIX timer or `timerfd`) therefore expires after more
guest work, and a program that times its own work sees shorter intervals.
Linux defines no cost for a context switch that Hermit could copy. What it
does define for `CLOCK_MONOTONIC` still holds. `clock_gettime` calls the
clock's `clock_get_timespec`
([kernel/time/posix-timers.c:1132-1148 at v6.9](https://git.kernel.org/pub/scm/linux/kernel/git/stable/linux.git/tree/kernel/time/posix-timers.c?h=v6.9#n1132)),
which for `CLOCK_MONOTONIC` is `posix_get_monotonic_timespec`, a read of
`ktime_get_ts64`
([:154-159](https://git.kernel.org/pub/scm/linux/kernel/git/stable/linux.git/tree/kernel/time/posix-timers.c?h=v6.9#n154),
[:1448-1451](https://git.kernel.org/pub/scm/linux/kernel/git/stable/linux.git/tree/kernel/time/posix-timers.c?h=v6.9#n1448)),
a clock that never goes back. Under the option Hermit's virtual time still
never goes back either: a thread's own work only adds to it, and a capped
scheduler charge only shrinks what a turn adds, never below zero. The
one-nanosecond floor further keeps a capped turn whose full charge is nonzero
from adding nothing.

Most of that cost comes from stopping the guest at the end of every slice,
and much of it is saved where no other thread could run. When every other
thread waits on a futex without a timeout, on a child or in `rt_sigsuspend`,
and no timer, sleeper, poll deadline or host call is pending, a slice end
cannot hand the turn to anyone, so Hermit lets the thread run on and processes
each end it skipped when the thread next enters Hermit (a system call, a
signal, a trapped instruction), at the same retired-branch count and in the
same order. The schedule, the virtual clock and the chaos draws are what a stop
at every end gives; `--eager-lone-preemptions` restores those stops, for
comparison, and `--summary` reports how many ends were taken without a stop.
Inputs from the host are the exception, as they are timed by the host either
way: a signal sent from outside (which can also wake a thread asleep in
`rt_sigsuspend`, a call that waits in the kernel) is seen at the next scheduler
turn, which may come later than a stop would have; and a thread killed from
outside with `SIGKILL` never has its skipped ends processed.
Slice ends while two threads can run still stop the guest. This applies to the
ptrace backend's precise timer, and not where the run uses `--target-timeslice`,
`--chaos-per-thread-slowdown`, `--imprecise-timers`, strict schedule boundaries,
turn-numbered triggers (`--stop-after-turn`, `--stop-after-iter`,
`--signal-at`), stack traces at preemptions or events, manual interrupt points,
a happens-before program, `--chaos-instruction-offset` (each offset is stepped
from a stop at its branch target) or `--gdbserver` (a debugger stopping the
thread between skipped ends would see Hermit's slice bookkeeping behind its
program counter).

Recording and replaying keep these savings. A recorded schedule
(`--record-preemptions-to`) holds each slice end taken without a stop as the
branches up to it, as for any slice end, but with no instruction pointer (the
guest was not stopped there) and marked `"stopless": true`. A schedule replay
(`--replay-schedule-from`) takes such an end without a stop too when the thread
again runs alone, and stops at every other end, where it checks the instruction
pointer the recording read. A thread that the replay switches to takes its
first end with a stop. A preemption replay (`--replay-preemptions-from`) takes
any recorded end at a retired-branch count without a stop when the thread runs
alone, except in a record with per-thread chaos rates, and except for a thread
whose recorded ends hold instruction offsets, which are stepped from a stop. A
record made before this mark existed replays as before. A record that holds the
mark is preemption record format 5, which an older Hermit, reading formats 1 to
4, refuses as a format it does not know before the guest starts. A record that
keeps no background starts, and so states format 3 (a schedule replay's
re-record, a prefix or edit of a schedule, an analyze or bisect trial), keeps
no marks either: its replay stops at those ends, which replays the run as
faithfully.

Which slice length exposes a race depends on the race: some need a preemption
in a short window (fine slices), others a long uninterrupted run of one thread
(coarse slices), and a seed sweep under one `--max-timeslice` explores mostly
one scale. `--chaos-timeslice-range=MIN..MAX` draws each slice's mean
log-uniformly from MIN..MAX virtual nanoseconds, so every run mixes all the
scales in the range, and caps every slice at MAX. Slices are still random
around the drawn mean, so some fall below MIN:

```bash
for seed in $(seq 1 32); do
  hermit run --chaos --sched-seed="$seed" --chaos-timeslice-range=1000..200000000 \
    -- ./target/debug/hello_race
done
```

The draws come from the scheduler seed, so a seed reproduces its run. The
range sets the run's `--max-timeslice` to MAX; give one or the other, and do
not combine it with `--target-timeslice`. It needs PMU-backed preemption. A
small MIN makes runs slower, because fine slices mean many preemptions.

Advanced investigations can save preemption decisions with
`--record-preemptions-to=FILE` and replay them with
`--replay-preemptions-from=FILE`. A record edited after it was recorded (for
example, with preemptions removed, as `hermit analyze --minimize` does) can
bring a thread to recorded slices that have already ended. A faithful replay
never does, so Hermit stops it there; add `--replay-edited-preemptions` to skip
those slices and go on with the first recorded slice still ahead, at its
priority (at a thread's start, the record's first priority holds until that
slice ends). `hermit analyze` adds it to the records it edits.
A record pins every slice end, so a run under `--chaos-timeslice-range`
replays from it exactly; give the replay the same range, or
`--max-timeslice=MAX`, so slices past the end of the record are capped as in
the recording.

Explicit `--replay-edited-preemptions` is exploration-only. It cannot be
combined with `--verify`, `--verify-strict`, or `--verify-run`, even when the
input record has no departure marker: an edited replay can omit a change point
while running, so matching output alone cannot certify faithful replay. Ordinary
unmarked faithful replay and verification remain supported. Analyze's separate
selfcheck requires both its input and recorded result to have no known omission.

A record with `replay_departed_at` is known to omit a replay change point.
Faithful history replay, global-schedule replay, boundary replay, and analysis
refuse it with `HERMIT_INCOMPLETE_REPLAY_RECORD` before using it as execution
evidence. Explicit edited history exploration may read it, with a warning, and
any new recording retains the omission fact. Copying, stripping timestamps,
normalizing, or interpolating a record cannot remove that fact. A retained count
refers to a source observation; it need not index a transformed output.

Newly written marked JSON uses the mandatory
`incomplete_preemption_record` version-1 envelope, which old readers reject.
The new reader also recognizes historical flat marked JSON and refuses its
faithful use, but cannot protect those already-written flat files from old
binaries. Unmarked legacy bytes and their admission policy remain unchanged;
a missing marker does not prove historical completeness. Marked records cannot
be serialized through an unversioned non-human format. No appended-data
detection or generic legacy binary roundtrip guarantee is implied.
A schedule replay (`--replay-schedule-from`) whose next recorded event goes
unmatched for a thousand observed events in a row releases the rest of the
schedule and runs on unguided. Unmatched polling retries neither count toward
that run nor break it. The replay's warning and the run summary's
`RELEASED replayed schedule at event #N` line number events without polling
retries, whose count can depend on host timing. A `--stacktrace-event` index,
like the desync and `Missed stack trace` lines, counts every traced event,
polling retries included, so a release's number is not a `--stacktrace-event`
index. A requested index the run never reaches is reported, not dropped. The
report comes in the run summary of a run that records or replays a schedule,
or before a replay stops the run under `--replay-exhausted-panic` or
`--die-on-desync`. A run that ends without a summary, such as a non-ptrace
backend's `--timeout`, does not report.

Under `--strict-schedule-boundaries` a `--stacktrace-event` index is a
position in the complete boundary stream instead, `TimesliceEnd` records
included. A replay refuses an index outside its stored stream when the run
starts, and the reports above do not apply to it. A recording counts the
stream as it writes it and, like any recording, reports an unreached index
in its run summary.

A replay reproduces the recording only if the guest reads the same host
files. The recorded schedule therefore keeps the set of host files the run
read (`host_inputs`: each path with its host identity and, for a regular file
of at most 32 MiB, the SHA-256 of its bytes, or for a directory, of the names
in it), and a replay (with
`--replay-preemptions-from` or `--replay-schedule-from`) prints one line for
each such file that changed since:

```text
HERMIT_HOST_INPUT_CHANGED_SINCE_RECORDING host input /etc/ld.so.cache changed since the recording (...); the run may diverge where the guest reads it
```

The comparison counts the files the guest opens and those the kernel opens
for its execs: each program image and its ELF interpreter (for a `#!`
script, the interpreter the kernel ran, not the script), each with the
identity it had when the guest opened it or the kernel ran it, so a file
replaced later in the run is reported against the one the guest used. An
interpreter Hermit cannot identify as the file the kernel mapped (on an
overlayfs root, for example) is left out, and a comparison then has no
evidence about it. Where Hermit may not follow `/proc/<pid>/map_files` (it
needs CAP_CHECKPOINT_RESTORE or CAP_SYS_ADMIN in the initial user
namespace), it identifies the interpreter by its inode number alone, so a
file of the same inode number that another filesystem or btrfs subvolume
exposes at the interpreter's name cannot be told apart. A file whose bytes
were hashed in both runs counts as changed only if its bytes differ, so a
file rewritten unchanged, as `ldconfig` rewrites `/etc/ld.so.cache`, is not
reported; a directory counts as changed only if the names in it differ,
leaving out the names the guest made or removed itself, so the `/tmp` Hermit
makes afresh for each run is not reported. A file that was not hashed (larger
than 32 MiB, changed again before the run ended, or on a backend whose tool
cannot name the guest's descriptors) counts as changed when its host inode,
size or modification time differs. Hermit reads the bytes and names only
after every guest process has exited, never while the guest runs: Linux
raises an inotify event for every open and read of a file, and a guest
watching the file's directory would see those reads. So a regular file whose
size changed is reported at the guest's first open of it, and any other
change when the run ends. Kernel objects (paths under
`/proc`, `/sys` and `/dev`, names that lead there, such as `/etc/mtab`, and
FIFOs, sockets and devices wherever they are, which hold no bytes to compare)
are left out, and so are paths the guest itself renamed, linked or unlinked,
or opened with `O_CREAT`. Under `--backend=dbt` nothing is recorded. The
line is information, not a refusal: Hermit does not make a changing
filesystem deterministic.

#### Discovery mode

For race discovery, sweep seeds with `--chaos-timeslice-draw=run`. Each run
draws its `--max-timeslice` once, log-uniformly from the recommended range
1000..200000000 virtual nanoseconds (1 us to 200 ms), as a pure function of the
scheduler seed, and is then an ordinary chaos run under it, so the seeds of a
sweep spread over the scales of the range, each seed at one scale:

```bash
for seed in $(seq 1 64); do
  timeout 600 hermit run --chaos --chaos-timeslice-draw=run --sched-seed="$seed" \
    --summary-json="$PWD/run-$seed.json" -- ./target/debug/hello_race
done
```

The recommended range is `RECOMMENDED_CHAOS_TIMESLICE_RANGE` in
`detcore-model/src/config.rs`, and `hermit run --help` states it; give
`--chaos-timeslice-range=MIN..MAX` to draw from another range. It spans the
fine timeslices at which races that the fixed default never reaches were found
(1 us to 5 ms) and the default itself (200 ms). The default is unchanged: plain
`--chaos` keeps the fixed 200 ms, and `--chaos-timeslice-draw=slice` (the
default draw) draws each slice's mean from `--chaos-timeslice-range`, which it
requires when given explicitly. The per-run draw is not the default because it
costs time and trades races:

- Fine draws mean many preemptions. On the repository's racy guests the p90
  run took up to about 120 times as long as at the fixed default; in a study of
  115 cases the draw took 4.4 times the run time in all, and 78 of its runs hit
  their time cap against 9. Size time limits for the fine end.
- Draws below about 3 us dilate guest-visible time by two to three orders of
  magnitude. At draws near 1 us the fixed charge for each committed scheduler
  turn (0.5 ms) makes up about 99% of guest-visible time
  ([#4813](https://github.com/rrnewton/hermit/issues/4813)), so a guest's own
  watchdogs can fire on threads that are making progress. Pass
  `--cap-preemption-charge` beside fine draws: it caps that charge (in that
  study it cut a control's watchdog aborts at a 1038 ns draw from 5 of 5 seeds
  to 1 of 5). It is refused where no timeslice ends by expiry
  (`--max-timeslice=disabled` without `--target-timeslice`), where no Detcore
  scheduler orders the threads (`--namespace-only`, `--strace-only`,
  `--no-sequentialize-threads`) and with the DBT backend, and it is never
  applied in record mode.
- A race that needs coarse slices is reached by fewer seeds than at the fixed
  default.
- It is meant for seed sweeps: without a seed every run is seed 0 and draws the
  same value.

`--summary-json` reports the drawn value (`preemption.max_timeslice_ns`), and a
`--record-preemptions-to` record stores it (`chaos_timeslice`). On a replay:

- A record that states a drawn value replays under it with no timeslice
  option, drawing nothing. It takes precedence over a range or
  `--chaos-timeslice-draw` the replay gives. `--max-timeslice=N` is accepted
  for the recorded N only when the replay passes no range or per-run draw;
  beside either, an explicit `--max-timeslice` is refused, whatever the
  record states. `--chaos-seeded-timeslice` is refused.
- A record that states none (one recorded at a fixed timeslice, or before
  records stated the draw) leaves the replay's own timeslice options in force.
  With `--chaos-timeslice-draw=run` the replay draws its maximum from its own
  scheduler seed, as a run without a record does; to replay such a record as it
  was recorded, give the `--max-timeslice` it was recorded with, or no
  timeslice option for the default 200000000.
- A range without the per-run draw beside such a record replays its slice
  ends under the range's MAX, as for any per-slice record.

`hermit analyze` runs all its trials under the drawn value its recordings
state (`--run1-preemptions`, `--run2-preemptions`), and `hermit bisect` all its
replays under the one its `--good` and `--bad` schedules state, passed as
`--max-timeslice=N`. Both refuse `--chaos-timeslice-draw=run` in their run
arguments (each trial would draw its own value), recordings that state
different values, a `--chaos-timeslice-range` in the run arguments beside a
recording that states one, and a `--max-timeslice` that differs from it. The
schedules analyze writes do not state the value: give a later `hermit bisect`
of them the same `--max-timeslice=N`.

The draw and the range need `--chaos` and PMU-backed preemption (the ptrace
backend; the others refuse them), and conflict with an explicit
`--max-timeslice`, with `--chaos-seeded-timeslice` and with
`--target-timeslice`. Each bound of a range must be at least 10 virtual
nanoseconds (one RCB at the default clock multiplier), and MIN no larger than
MAX.

### Verify Mode

Verification is an option to `run`, not a separate top-level command:

```bash
hermit run --verify -- /bin/echo reproducible
```

Hermit runs the guest twice and compares observable output, including stdout,
stderr, and its internal deterministic execution log. Verification fails if
the compared observations differ or if the guest exit status is not allowed.

The verifier emits the first run's guest stdout and stderr once when verification
succeeds, or when both runs agree on both output streams. This also applies to
DBT: a log-only or branch-count divergence no longer suppresses agreed guest
output. The divergence remains a failure. If either stream differs, the labelled
comparison shows the difference instead of choosing one run's output. Errors
writing the agreed output are still errors, even if the comparison report was
already written.

The default log comparison is `Stripped` (DBT retains its `Canonical` comparison):
it can erase numbers, addresses,
temporary paths, and time values from selected messages. It is a fast
diagnostic, not strict determinism. Use
`--verify-strict --verify-json REPORT.json` when a canonical strict result is
required.

Matching verification logs are temporary by default; divergent comparisons
retain both. Hermit keeps the newest 64 implicitly retained failed comparisons
under `$XDG_STATE_HOME/hermit/verify-failures` (or a bounded directory under the
system temporary directory when no state directory is available) and prints
their final, readable paths. A `hermit log-diff` reading one of those paths
prevents its comparison directory from being retired until the read finishes.
`--keep-logs` retains logs whatever the verdict, without that automatic
retirement, and Hermit prints only the final, readable paths. After a match it
keeps only the first run's log, as the golden log; the second run's log, which
compared equal to it under the selected comparison policy, is deleted. The two
raw files need not be byte-identical: every policy removes the real wall-clock
prefix and normalizes host addresses, and the default policy (without
`--verify-strict`) ignores more, so the golden log carries the first run's
values wherever the comparison looked past a difference. After a divergence, a
no-result, or an error it keeps both logs, or only the first run's when
verification stopped before a second run. The default
destination is `$XDG_STATE_HOME/hermit/verify-logs`, normally
`~/.local/state/hermit/verify-logs`, and `--verify-log-dir=DIR` selects another
durable directory. `--print-verify-logs` instead copies the first run's captured
log to stderr; it does not retain either file.

Without `--keep-logs`, the two logs are temporary files in the system temporary
directory, or in `$HERMIT_VERIFY_TMPDIR` when that is set and not empty. The same
applies to the record and replay logs of `hermit record start --verify`. A verify
that is killed before it finishes leaves both logs there. A caller that kills
verify runs at a deadline can set `HERMIT_VERIFY_TMPDIR` to a directory it
removes afterwards. A directory that cannot hold the logs is reported as an
error naming `HERMIT_VERIFY_TMPDIR`. Hermit does not pass that variable on to the
guest unless it is named with `--env`, so unlike changing `TMPDIR` it does not
change what the guest sees.

`hermit log-diff LOG` prints the canonical INFO stream from one retained log.
The default `--record-envelope=all-records-v1` preserves every parsed record.
For a DBT evidence log, select
`--record-envelope=dbt-evidence-transport-v1` explicitly to exclude only the
transport's self-description records. This is an offline inspection option;
live `--backend=dbt run --verify` uses the common verifier with the existing DBT
execution and evidence adapter. It compares both runs with `all-records-v1` and
includes DBT's branch counts and process statistics before publishing the common
verdict. Selecting the narrower envelope there would exclude records compared
today, so it remains an offline inspection option. A decode, materialization, or
statistics failure after a physical DBT run records that run as failed, never as
a match; failure in run 1 prevents run 2 from starting.
`hermit log-diff LEFT RIGHT --json REPORT.json` compares
the same canonical INFO stream and atomically records a one-line result. Its
`comparison.record_envelope` field names that versioned policy, alongside the
first divergent scheduler turn and virtual nanoseconds when the logs contain
that position. An empty or unreadable comparison exits with an error rather
than reporting a match.

For a two-log comparison, add `--print-logs` to print both selected streams to
stderr exactly as the comparator receives them. The output names the active
policy: `Deterministic` for the default DETLOG/scheduler-COMMIT subset,
`Stripped` when `--unsafe-strip-lines` applies its lossy substitutions, or
`Canonical` when `--canonical-info` selects the INFO stream and canonicalizes
marked host addresses. `--canonicalize-host-addresses` applies that same
host-address canonicalization to the other comparisons, reported as
`Deterministic with Canonical host-address normalization`: each explicitly
marked `<hostaddr 0x...>` value becomes a per-run ordinal, so identity, order
and aliasing are still compared while an ASLR shift between two processes is
not a difference. This output is produced by the shared comparator path,
after wall-clock-prefix removal, line filtering, message selection, and any
requested substitutions.

The guest must be idempotent. A first run that modifies an input file,
database, cache, or other host-visible state can legitimately change the
second run. Use disposable or resettable inputs for verification.

By default both runs must succeed. Expected failures can be checked with:

```bash
hermit run --verify --verify-allow=failure -- PROGRAM
hermit run --verify --verify-allow=both -- PROGRAM
```

`--verify-allow` changes which guest statuses are accepted for comparison. It
does not turn a nonzero guest status into a successful guest exit status for
calling scripts.

The two runs can also be started separately, for example in parallel, and
compared afterwards:

```bash
export HERMIT_EPOCH=2026-01-01T00:00:00Z
hermit run --strict --verify-strict --verify-run run1 -- PROGRAM &
hermit run --strict --verify-strict --verify-run run2 -- PROGRAM &
wait
hermit log-diff --verify-json report.json run1 run2
```

Each `--verify-run` writes one execution's evidence to a new directory: its
verification log, the guest's output, and a `verify-run.json` record of how it
ended. `log-diff --verify-json` compares two such directories exactly as
`--verify` compares its two runs, writes the same report, shows the guest's
output as `--verify` shows it, and exits with the same status; it accepts
`--verify-allow`, and `--verify-log-dir DIR` keeps the compared logs in DIR as
`--verify --keep-logs --verify-log-dir DIR` keeps them. Both runs must use
the same comparison flags and the same epoch, which `--verify` would have
given both; the comparison refuses directories that differ in either. Where
`--verify` would not have started its second run (the first failed, or its
status is not one `--verify-allow` admits), the second directory need not
exist.

### Record And Replay

Record/replay is experimental. Unlike deterministic `run`, record mode captures
nondeterministic events from an execution and stores the data needed to replay
them.

Create a recording:

```bash
hermit record start -- /bin/echo recorded
```

The command prints a recording ID. Recordings default to
`$XDG_CACHE_HOME/hermit`, normally `~/.cache/hermit`. Select another directory
with `--data-dir=DIR` or the `HERMIT_DATA_DIR` environment variable.

For command-line compatibility with `hermit run --strict`, recording also accepts
`--strict`. Record/replay already uses a single serialized configuration, so the flag
does not change recording semantics. The direct form is an alias for `record start`:

```bash
hermit record --strict -- /bin/echo recorded
```

List recordings:

```bash
hermit record list
hermit record list --json
```

Replay a recording to completion without a debugger:

```bash
hermit replay --autopilot RECORDING_ID
```

Omit the ID to replay `last`, the newest `hermit record` (or `hermit run
--record-networking`) recording; see "Run Manifests" for every way to name a
recording:

```bash
hermit replay --autopilot
```

Without `--autopilot`, `hermit replay` starts a GDB client and a replay
gdbserver. This requires `gdb` on `PATH`. Use `--gdbserver-port=PORT` and
`--gdbex='COMMAND'` to customize that session.

Recording management commands are:

```bash
hermit record rm RECORDING_ID
hermit record clean
```

`hermit record start --verify -- PROGRAM` records and immediately replays the
command. On a successful replay it deletes the temporary recording, so do not
use this form when the recording must be retained.

The recording and its replay can also be run as separate commands and
compared afterwards, as `hermit run --verify-run` splits `--verify`:

```bash
hermit record start --verify-strict --verify-run rec -- PROGRAM
hermit record start --verify-strict --verify-run rep --verify-replay-of rec -- PROGRAM
hermit log-diff --verify-json report.json rec rep
```

`rec/recording` holds the recording. Give both commands the same options; the
comparison writes the report `record start --verify --verify-json` writes.

To record a racy failure, record under chaos scheduling and try seeds until one
fails, then replay that recording under GDB:

```bash
for seed in $(seq 1 64); do
  hermit record start --chaos --seed="$seed" -- PROGRAM || break
done
hermit replay
```

The recording stores its schedule, and replay follows it. `--seed` (also
spelled `--sched-seed`, as for `hermit run`) seeds only the scheduler; the
guest's random numbers are recorded. Recording runs under a different
configuration from `hermit run`, so a seed that fails under
`hermit run --chaos` does not necessarily fail under `hermit record`.

Replay depends on the captured data and compatible Hermit behavior. Keep the
recording directory intact and prefer the same Hermit revision for recording
and replay.

### Analyze Mode

`hermit analyze` is an advanced workflow for finding and comparing chaos-mode
executions that pass and fail. For example, search for a nonzero exit and write
a report:

```bash
hermit analyze --search --target-exit-code=nonzero \
  --report-file=analysis.txt -- ./target/debug/hello_race
```

Run `hermit analyze --help` before using this workflow. Search, minimization,
and schedule replay can perform many guest executions and require a reliably
reproducible target condition.

Analyze keeps every run's schedules, logs, output and reproducer command in a
workspace, a `hermit_analyze*` directory it creates in `--tmp-dir`, or else in
`$TMPDIR` (`/tmp` when TMPDIR is unset); the paths it prints name that
directory. Analyze removes the workspace when it exits after its report, and
when SIGTERM, SIGINT or SIGHUP stops it; after an error it keeps it, since the
error can name trial files in it (a trial that did not finish, say), and a
budget's partial report keeps it too (see below). A stop
signal ends the run in progress and starts no other; analyze then removes the
workspace and that run's own temporary directories, and dies of the signal, so
a shell reports 128 plus its number (143 for SIGTERM). Pass `--keep-tmp` to
keep the workspace, for example to rerun a reproducer; `--verbose` keeps it
too, since the reproducers it prints name its files, and so does
`--trial-log` above `warn`. The report file (`--report-file`) and the
`--tmp-dir` directory itself are never removed.

Every run analyze starts logs at the `warn` level to analyze's stderr, as
`hermit run` does. `--trial-log=info` or `--trial-log=debug` writes each run's
log at that level into a file of its own in the workspace (`<run>.log`), and
`--trial-log=off` drops the runs' warnings but keeps their errors, among them
the reason Hermit gives when it refuses a trial; `--verbose` prints analyze's
own detail and leaves the runs' level alone. Without `--trial-log`,
`--guest-log` (or HERMIT_LOG) gives the level. A debug log can grow to
gigabytes per run and make the run several times slower, so a trial that a
bound stops gets through less of the program. `--selfcheck` compares the
target run's log with its replay's, so the runs whose logs it may compare (the
target run, the `--search` run that becomes it, the `--minimize` runs and the
replay) log at `debug` or above into their files, whatever `--trial-log` says.

While analyze runs, a bisection trial's schedules, as large as the endpoints'
(hundreds of MB for a few million events), are removed as soon as the trial's
outcome is read, and every run's recording with event times once analyze has
written it without them. `--keep-tmp` keeps them as well; `--verbose` does
not, so the reproducer it prints for each trial needs `--keep-tmp` to run
afterwards. `hermit bisect` keeps them.

Analyze cannot clean up after SIGKILL, nor after a stop signal it does not
handle: a signal it inherits as ignored stays ignored (SIGINT in a shell's
background job, SIGHUP under `nohup`), and an analyze that is PID 1 of its PID
namespace, as in `docker run IMAGE hermit analyze ...` without `--init`, keeps
the default actions, which Linux discards for a namespace's init. Analyze also
leaves the stop signals to kill it at once, as before, when `/proc` is not its
own PID namespace's (analyze started by another process inside `unshare -pf`
without `--mount-proc`) or when it inherited more than 64 children, since it
could not then find the run to stop, and where it cannot open a pidfd, send
a signal through one or wait on one (Linux before 5.4, or a seccomp filter
that refuses `pidfd_open`, `pidfd_send_signal` or `waitid`), since it signals
the run only through a pidfd: a process ID it found can be reaped and given to
another process before the signal goes out. A stop it cannot pass on to the
run (no file descriptor left to read `/proc`, or a pidfd call that fails) is
not lost either: analyze says which call failed and dies of the signal as it
would without its handler, the run dies with it, and the workspace and the
running trial's private temporary directory stay, since such a death unwinds
nothing.
`hermit bisect` keeps its workspace and names it when it exits.

A replayed or spliced schedule can leave the guest spinning or waiting indefinitely.
`--trial-timeout=SECONDS|auto|off` bounds runs after the initial target attempt.
The default `auto` uses the initial completed run and completed target/baseline
endpoints: max(60 seconds, 10 times the slowest measured run), rounded up to
whole seconds. Analyze prints the bound when it changes. A shorter `--timeout`
in the run arguments still wins and also bounds the initial run. With
`--run1-preemptions` the initial run is a replay of the record, measured like
a seed's, which an explicit `--trial-timeout=SECONDS` bounds too. On a
backend that does not enforce `--timeout`, `auto` gives no bound and an explicit
number is refused; bound the whole analysis from outside there.

A timed-out trial cannot certify a causal report: it establishes neither the
target nor the baseline outcome, even if it captured stacks. Analysis returns
exit 124 without saving a successful report; diagnostics from previously
completed runs may already have been printed. This also applies when the
default non-chaos baseline times out; analyze
does not replace it with a prefix search. Retry with a larger trial bound if the
guest can finish. `--trial-timeout=off` removes analyze's time and log bounds,
while leaving a user-supplied `--timeout` in force. The log files bounded
trials write under `--trial-log` above warn or `--selfcheck` are capped at
max(64 MiB, 10 times the largest measured endpoint log), within
`HERMIT_LOG_MAX_BYTES`; setting that variable to `0`
lifts both file caps. These host resource limits may stop a trial on a slow
machine; they never change a stopped trial into a completed outcome.

A trial refused by Hermit (`HERMIT_POLICY_REFUSAL`, exit 122) stops the search
without establishing a program outcome for that trial. It cannot replace a
completed baseline or certify a causal report. The error names the trial and
its log file, or stderr when no file was requested. Inspect the named refusal
and use a supported operation or configuration before retrying. An ordinary
guest exit 122 remains a completed guest outcome, classified by the target
criteria; its numeric status alone is not a refusal.

By default analyze bisects between the failing schedule and a passing one from
another run. When the two runs differ in events only one of them executed (for
example timeslices that end at other points, or extra iterations of a polling
loop), no single swap may separate the outcomes. `--prefix-search` searches
within the failing schedule instead: each trial replays its first K events and
then schedules without chaos, and a binary search finds the event at which the
outcome flips. The continuation runs at the run's own `--max-timeslice`; a race
found only at a short timeslice often shows without chaos at that timeslice
too, and then the search continues at 200 ms instead
(`--continuation-timeslice`). The report shows the stack of the thread the
failing schedule runs at that event and of the thread that runs there
otherwise. When both continuations run the same thread there, the decision is
how long that thread runs: the report then shows its stack, and the stacks of
the threads that run after it with and without the event:

```bash
hermit analyze --prefix-search --run1-seed=1 --target-stdout='FAILED' \
  --target-exit-code=any --report-file=analysis.json -- --chaos -- PROGRAM
```

Analyze runs every trial under the `--expired-wait-charge` rule of the
recording it starts from, or `each` for a chaos target it records itself. A
baseline schedule recorded by a plain `hermit run --record-preemptions-to=...`
stores `once`, so analyze refuses it beside a chaos target. Re-record it with
`--expired-wait-charge=each`, or, if it is the target, pass
`--expired-wait-charge=once` in the run arguments.

Analyze's exit status says what it concluded. A conclusion is never reported
as a failure of Hermit; only a failure to conclude exits 125, under a class
line that says which. The class line is the first line on stderr:

| Exit | Line on stderr | Meaning |
| --- | --- | --- |
| 0 (or `--success-exit-code`) | none | The report names the critical events (`"verdict": "critical_events"`). |
| 0 (or `--success-exit-code`) | `HERMIT_ANALYZE_VERDICT class=reproduced-without-schedule` | The target is reproduced without the target schedule: without chaos the scheduler alone meets the target, so the prefix search localizes no event (which does not show that no event decides the outcome); debug that run directly. The report says why (`"verdict": "reproduced_without_schedule"`) and names no event. |
| 2 | `HERMIT_ANALYZE_PRECONDITION class=target-not-reproduced` | The target run, the failing endpoint of a bisection, or `--minimize`'s last record does not meet the target criteria. |
| 2 | `HERMIT_ANALYZE_PRECONDITION class=baseline-not-reproduced` | The baseline meets the target too, or the baseline the user chose does not finish. |
| 2 | `HERMIT_ANALYZE_PRECONDITION class=no-prefix-continuation` | No continuation timeslice allows a prefix search. |
| 2 | `HERMIT_ANALYZE_PRECONDITION class=endpoints-not-separable` | `hermit bisect` only: its search established no exact one-swap pair of schedules with opposite outcomes between `--good` and `--bad`. Analyze attempts a prefix search within the target schedule instead. |
| 2 | `HERMIT_ANALYZE_PRECONDITION class=endpoints-identical` | `hermit bisect` only: `--good` and `--bad` contain identical event traces. |
| 2 | none | clap's usage error: the command line did not parse. |
| 120 | `HERMIT_ANALYZE_PARTIAL class=time-budget` or `class=trial-budget` | A budget (`--time-budget`, `--trial-budget`) ended the analysis, which reports its best answer so far (see below). |
| 122 | `HERMIT_POLICY_REFUSAL class=policy-refusal` | Hermit refused the target run, or analyze or bisect refused its options or schedules. |
| 123 | `HERMIT_LOG_CAP class=log-cap` | `--max-log-bytes` ended the analysis. |
| 125 | `HERMIT_INTERNAL_FAILURE class=replay-not-reproducible` | `--selfcheck` found a difference, or a jittered schedule never replayed the same way twice: replay, Hermit's or this host's (`hermit host-capabilities` says whether its branch counter is exact), did not reproduce a run. |
| 125 | `HERMIT_INTERNAL_FAILURE class=search-did-not-converge` | The event-level search did not converge within its passes. |
| 125 | another `HERMIT_INTERNAL_FAILURE class=...` | Hermit failed. |
| 128 + N | none | Signal N (SIGTERM, SIGINT or SIGHUP) stopped the analysis. |

Exit 2 means that the inputs do not give analyze what it needs, as `diff` and
`grep` exit 2 when they cannot do what was asked; the message says what to
change. `--success-exit-code` refuses 2 and 120 to 127 for that reason. `hermit
bisect` checks that its `--good` schedule misses the target and its `--bad`
schedule meets it, and exits 2 with the same lines when one does not.

A report file (`--report-file`) has one schema for every verdict: read its
`verdict` first. `critical_events` fills `critical_event1` and
`critical_event2` (and `deciding_event` for a timing decision);
`reproduced_without_schedule` leaves them out, and the racing accesses
(`racing_accesses`) too, and `header` says why. Its
`verdict_reason` says what the scheduler alone shows. When the trials keep
timing settings from the run arguments (`--pipe-wake-latency`,
`--scheduler-turn-cost` other than the default charge), analyze runs one more
trial, the empty prefix without them at the first continuation it tried: if it
misses the target, the
reason is `{"cause": "timing_semantics", "options": [...], "alternatives":
[...], "trial": "..."}`, the settings cause the outcome, a two-seed search
can end the same way, and a new failing run under the alternatives may have
an event that decides it. Otherwise (it meets the target too, or it is not
run because a `--run1-preemptions` recording fixes the settings) the reason
is `{"cause": "scheduler_alone", "timing_in_force": [...]}`, and without
such settings `{"cause": "scheduler_alone"}`. A cause a later build adds
reads as `unknown`. A report from before the field names the critical
events.

A long analysis can be bounded, and it then reports its best answer so far
instead of nothing. `--time-budget=DURATION` (`1800`, `30m`, `2h`) bounds the
invocation's wall-clock time, and `--trial-budget=N` the number of runs it
starts. The budget is checked before every run, and each run's `--timeout` is
shortened to the time left; a run the budget cuts short is thrown away rather
than counted as not meeting the target, so the budget decides only when
analyze stops, never what it finds. When it runs out, analyze exits **120**
after the stderr line `HERMIT_ANALYZE_PARTIAL class=time-budget` (or
`class=trial-budget`) and a report that says it is partial, why, and which
phase was running:

- The default search reports the closest pair of schedules it found, a
  schedule whose replay misses the target and one whose replay meets it
  (`interval_passing.events` and `interval_failing.events` in the workspace),
  their edit and swap distances, and the first event at which they differ: the
  deciding event is that one or a later one. It takes the two schedules'
  stacks there when two replays as slow as the slowest endpoint run fit
  min(120 s, 5% of `--time-budget`) after the budget ran out, and says so when
  they do not.
- `--prefix-search` reports its bracket. Short prefixes miss the target and
  long ones meet it, so the report names the longest prefix whose replay
  finished and missed, the shortest whose replay met it, and the range of
  events between them that holds the deciding event.
- A budget that runs out after the search finished, during its final
  replays, reports no decision: analyze names one only once those replays
  repeat the search's outcomes, so the report's event fields are absent and
  `partial.stacks` says why; `--resume` runs them. A partial report never
  names a decision, so a script reads `partial` first.

In a report file such a report's `verdict` is `partial`, and `partial` holds
what it found. While the default search runs, `critical_event1` and
`critical_event2` name where its closest pair first differs, which is the
deciding event or one before it; otherwise the event fields are absent.

Analyze stops itself only between runs, and the default search's computation
between runs and the stack replays after the budget can take a little longer,
so give a harness's own kill bound (a test timeout, `timeout(1)`) room above
`--time-budget` plus min(120 s, 5% of it), for example a few minutes more.
Analyze then stops itself first and writes its partial report and resume
command, which an outer kill would lose.

Analyze writes its progress into the workspace as it goes: every finished
run's outcome (`analyze-journal.jsonl`), what the analysis is of
(`analyze-checkpoint.json`) and the search's interval or bracket
(`analyze-progress.json`). When a budget ends it, the workspace is kept even
without `--keep-tmp`, and the report gives the command that continues it: the
same arguments plus `--resume=WORKSPACE`. A resumed analysis runs again from
its start, and every run in the journal takes its recorded outcome instead of
running. A run its own bound stopped, or Hermit refused, is journaled too, and
resumes as the same error, which ends the analysis there again: such a run has
no outcome. The default search restarts at the pass whose interval the
progress names, from that interval's two schedules in the workspace, rather
than replaying its earlier passes, which would need every trial's schedules.
So the resumed analysis makes the same decisions and reports what an
uninterrupted analysis would. It refuses (exit 122), naming each difference,
when the hermit binary, the guest program, the run arguments, the target
criteria, the search options and `--max-log-bytes`, the record or schedule
files given as runs, or `--analyze-seed` differ from the recorded ones; to
start over anyway, run the command without `--resume`. With `--image` in the
run arguments, `--resume`, `--time-budget` and `--trial-budget` are refused
(exit 122): the image reference can name another image by the next invocation,
and the program runs from the image's root, so analyze cannot show that a
resumed analysis has the same guest. A workspace that `--keep-tmp` kept, or
that a SIGKILL left, can be resumed the same way. A resumed analysis that
errors or is stopped keeps the workspace, whatever its retention, so that it
can be resumed again.

```bash
hermit analyze --prefix-search --time-budget=30m --run1-seed=1 \
  --target-stdout='FAILED' -- --chaos -- PROGRAM          # exits 120 if the budget runs out
hermit analyze --prefix-search --time-budget=30m --run1-seed=1 \
  --target-stdout='FAILED' --resume=/tmp/hermit_analyzeXXXX -- --chaos -- PROGRAM
```

#### Racing accesses

The report ends with a "Racing accesses" section (`racing_accesses` in a
`--report-file`; `--no-racing-accesses` leaves it out). The final replays on
each side of the deciding event record an access trace, and analyze compares
the two from that event on. Its `status` says how far the evidence goes:

| Status | Meaning |
| --- | --- |
| `found` | The evidence singles out one pair of conflicting syscalls on two threads whose order flips between the runs. |
| `timing` | No conflicting pair; two timed calls whose return order flips. |
| `candidates` | Up to three ranked candidates, none singled out; each says what differs between the runs. |
| `found_by_intervention` | Exactly one candidate whose reversal changes the outcome while an unedited control does not. |
| `none_found`, `unavailable` | Neither, with the reason. |

A rank is not a verdict. Candidates are listed by kind of evidence: a thread's
change of course that fed the outcome's input first, then waits in flight
across the deciding event, then order flips the evidence supports, then the
rest. Analyze checks each candidate by replaying one run with the candidate's
turn order reversed, beside a control replay without the reversal, and the
candidate's `Check:` line (`check` in the report file) says what that showed:
`decides` (the reversal changed whether the run meets the target, and the
control did not), `does not decide`, `not realized` (the replay could not
follow the edited schedule; a timed sleep, for example, cannot return before
its deadline) or `not checked`, with how many events the edit moved and which
threads' events they crossed. A candidate whose check does not decide ends
"not shown to decide", whatever its rank. Only `found_by_intervention` says an
order decides the outcome. In a report file that sentence is not part of a
candidate's `evidence`: read its `check.verdict`, which is `decides` only for
a candidate shown to decide.

When the checks ran and none decides, the report says so after the
candidates, and keeps the cases apart. A reversal that ran as edited and left
the outcome as it was clears its candidate, but only as far as one turn
reversal in one run goes: when all of them did, the outcome probably does not
turn on any one of these candidates' orders alone, and may turn on an order not
ranked here, an order in memory that syscalls do not show, one outside the
trace, or several together. A reversal that could not be replayed as edited
(`not realized`), one never tried because its control did not reproduce the
run, and a candidate not checked leave it untested, not cleared. Either way,
nothing says the right pair is among the candidates. The report then lists what to try next: `--minimize`, so fewer
chaos interventions surround the deciding event; a wider `--race-window` when a
trace was cut short; and the unrealized checks' replays
(`racing_candidate_N.repro`), which show where each left its edit. A report
file carries the same information in the candidates' `check.verdict` values.

For example, an RPC server's shutdown removes a client in memory just before
the server's default handler looks the client up. No syscall shows that order,
so the first candidate is the handler thread's change of course against the
call the deciding event runs. The two run in the same order in both runs (in
the baseline the order is measured at the handler's call of the same number,
which there takes another course), and an edge Hermit observes orders them, so
the candidate is listed for the change rather than for an order flip, and its
check could not replay the reversal (stacks and part of the Check line left
out):

```text
Racing accesses: thread 3's read(5, 0x7fffffffec48, 4) returns 4 in both runs but reaches different data; it takes its data from a thread whose calls change course after the deciding event, and no access Hermit sees changes order around that change.
Candidates, ranked (each says what differs between the runs; a rank is not a verdict, and a candidate decides only where its check says so):
  1. thread 3's read(5, 0x7fffffffec48, 4) returns 4 in both runs but reaches different data; it takes its data from thread 9's write (#716), and thread 9's calls change course at its call 5 (#704); the deciding event 668 runs thread 7's read; the two run in the same order in both runs (the changed call's counterpart enters at #703 in the baseline); ordered in the target by futex wake thread 7 #684 -> thread 9 #701; it is listed for that change, ahead of order flips, and no access Hermit sees changes order around it, so what decides may be an order in memory that syscalls do not show; thread 3's read is 24 events before the outcome and reaches it through program order; not shown to decide.
     thread 7  #668  read(7, 0x7ffff73fde47, 1) = 1 (its return)
     thread 9  #704  write(2, "server: client 3 not found, no handler", 39) = 39
   Check: not realized: reversing this turn order (moving 8 events of thread 9 before thread 7's turn at #668, crossing thread 3 (1 event), thread 5 (3 events)) in the target run cannot be replayed as edited: the replay ran (...) at event #659, where the schedule has (...) (racing_candidate_1.repro).
```

Here thread 7 is the shutdown, thread 9 the default handler and thread 3 the
client. The candidate names the right roles, but nothing in the two runs shows
that their order decides the failure.

## Common Run Options

### Scheduling And Determinism

| Option | Effect |
| --- | --- |
| `--strict` | Uses the current deterministic defaults and refuses a run that would draw an input from the host: pass `--epoch` (or set `HERMIT_EPOCH`), and do not use `--seed-from=SystemRandom`. |
| `--allow-unsupported-syscalls` | Run-only compatibility escape hatch that permits Unsupported syscalls to reach Linux and prints a warning. A successful exit does not establish complete deterministic execution. |
| `--passthru-opt` | Use the reduced syscall subscription set for performance. Requires `--allow-unsupported-syscalls`; unlisted PassThrough syscalls bypass Detcore, weakening deterministic accounting. Unsupported syscalls remain subscribed so the explicit policy is observable. |
| `--untrap-passthrough-syscalls` | Off by default. Lets `getppid`, `umask`, `getgroups`, `capget`, `sched_get_priority_max`, `sched_get_priority_min` and `get_robust_list`, which Detcore always passes through unchanged and which never wait, run without a tracer stop. They then charge no virtual time, log no DETLOG line, are not counted in the run summary (its syscall histogram says so) and end no timeslice, so the clock, schedule, turn numbers and log differ from a run without the flag, the same way in every run with it. Ptrace backend only; refused with `--strict`, `--namespace-only`, without RCB time (`--max-timeslice=disabled`, `--no-rcb-time`, or a host without usable performance counters), with a preemption record to write or replay (`--record-preemptions`, `--record-preemptions-to`, `--replay-preemptions-from`, `--replay-schedule-from`, whether given on the command line, in a `--config` file or by a `--from` manifest), `--happens-before`, `--verify-run` and in `hermit analyze`. |
| `--no-sequentialize-threads` | Lets Linux schedule guest threads concurrently. This weakens schedule reproducibility. |
| `--no-deterministic-io` | Disables Hermit's deterministic short-I/O completion behavior. |
| `--chaos` | Uses seeded randomized deterministic scheduling. |
| `--sched-seed=N` | Selects a reproducible chaos schedule. |
| `--chaos-seeded-timeslice` | With `--chaos`, draws `--max-timeslice` from the scheduler seed (two in five seeds 1000 to 2000 virtual nanoseconds, two in five 1000000 to 200000000, one in five between) and prints the value. |
| `--target-timeslice=N` | Ends a logical turn at the first syscall boundary after N virtual nanoseconds. |
| `--max-timeslice=N` | Sets the maximum virtual time slice (minimum one scaled RCB) and requires PMU support. |
| `--max-timeslice=disabled` | Disables PMU timer preemption. |
| `--preemption-timeout=N` | Deprecated alias for `--max-timeslice=N`. |
| `--expired-wait-charge=each\|once` | How a run of consecutive turns that each end an expired timed wait is charged while another thread is queued: `once`, one scheduler turn for the run (the default outside race search), or `each`, a turn per wait (the default under `--chaos` or a heuristic that does not select by priority). |
| `--cap-preemption-charge` | Charges a turn that ends an expired timeslice at most the time its thread ran since its previous turn. |
| `--chaos-timeslice-range=MIN..MAX` | With `--chaos`, draws each slice's mean log-uniformly from MIN..MAX virtual nanoseconds and caps slices at MAX (the run's `--max-timeslice`). Not with `--chaos-seeded-timeslice`, which draws the maximum instead. |
| `--chaos-timeslice-draw=run` | Discovery mode: draws the run's `--max-timeslice` once from the range (without one, the recommended 1000..200000000), from the scheduler seed. Fine draws make runs much slower. |
| `--chaos-timeslice-draw=slice` | The default draw: each slice's mean from `--chaos-timeslice-range`, which it needs; what a range alone does. |

A thread that polls with timed waits shorter than a scheduler turn's charge
(`--scheduler-turn-cost`) would otherwise burn a full charge of virtual time
per poll while other threads wait. Under `--expired-wait-charge=once` a run of
turns that each end an expired wait, by one thread or several, is charged
once, so time moves as it would while those threads slept. Only a futex wait
answered `ETIMEDOUT` and a sleep already due count as expired waits; a
`ppoll`, `epoll_pwait2` or `pselect6` timeout is still charged each time. The
rule changes the guest-visible clock of such runs, so a recorded schedule
stores it, a replay that leaves the option out takes the record's, and a
replay under the other rule is refused. A record that stores no rule replays
under `each`; that includes schedules written by Hermit builds from the
middle of the rule's landing (commits `027e47703` to `90bccf201`), which
charged runs once without storing it. Two changes that came with the rule
apply to every `--replay-schedule-from` replay, whatever rule it uses: a
switch the schedule makes to a thread whose timed wait has already passed
wakes that thread for the switch, and a replay's own switch turns no longer
count as the previous turn when Hermit decides whether a signal to the
running thread costs a turn. Both bring a replay closer to its recording, but
a replay of an older schedule can differ from what an earlier Hermit did.

`--no-sequentialize-threads` is useful for compatibility experiments and
workloads such as virtual machines that need real host parallelism. It removes
Hermit's strongest control over thread interleavings. Results may still benefit
from virtualized time and randomness, but the guest schedule is no longer
fully deterministic.

With the default `none` scheduling heuristic, runnable threads at the same
priority are selected in deterministic round-robin order. A thread is placed at
the back of its priority level after a committed scheduler turn. Therefore, if
`N` same-priority threads remain runnable and keep reaching intercepted yield
or synchronization boundaries, no thread waits behind more than `N - 1` other
runnable turns. The four-thread fairness test observes a maximum worker-progress
gap of three and also checks bounded-buffer producer progress and `RwLock`
writer completion across five identical runs.

This is a scheduler-turn guarantee, not a wall-clock deadline or a promise that
all user-space locking policies are writer-fair. Blocked threads are absent from
the run queue, higher priorities run first, polling operations use deterministic
backoff, and external I/O completion can add delay. Hermit does not change a
standard library's reader/writer-lock preference policy. PMU preemption bounds a CPU-only
time slice; with `--max-timeslice=disabled`, a thread that never reaches an
intercepted event can starve its peers. No separate fairness flag is needed for
the default equal-priority policy.

### Exclusive-Loop Rewrite (aarch64)

On aarch64, a load/store-exclusive retry loop (`LDXR`, then `STXR`, then a
branch back on failure) retries whenever an exception clears the exclusive
monitor between the two, so the same work can retire a different number of
branches. `--rewrite-exclusives` rewrites canonical loops in the guest
executable into single LSE atomic instructions before the run, which cannot
retry. It is preprocessing for the ptrace backend, not a backend.

| Option | Effect |
| --- | --- |
| `--rewrite-exclusives` | Rewrites the loops it can show are code. |
| `--rewrite-exclusives-also=FILE` | Also rewrites FILE, a shared library the guest loads. Repeatable. |

Only mapping symbols prove where code is: the AArch64 ELF ABI has the
assembler mark every switch between code (`$x`) and data (`$d`), so the
rewrite takes only what they mark as code. A file, or an executable section,
with no mapping symbols, such as every stripped file, is left unchanged and
named: the run's diagnostics and its summary (`exclusive_rewrite_unmarked`)
list it, and `hermit rewrite-exclusives --list-refusals FILE` lists each of
its loops as `no-mapping-symbols`. A section that holds a mapping symbol of
another kind (AArch32's `$a` or `$t`, or a `$` name Hermit does not know) is
not proved by them and is left unchanged, its loops listed as
`unsupported-mapping-symbol` with the symbol's name.

An unwind table (`.eh_frame`) does not prove code either: it covers whole
functions, including any table a function branches over or keeps after a call
that does not return, so a stripped file is left unchanged whatever its unwind
table covers.

A loop the file's code may read as data is left unchanged as well, listed as
`literal-inside-the-sequence`. Hermit follows every PC-relative address the
code forms (`LDR` literal, `ADR` and `ADRP`, through the `ADD`s and loads that
complete it) to each use of it. An access whose address and width the
instruction gives protects those bytes; any other use, such as an index
register, an SVE access, the address copied, stored or passed to a call, or
still held where the code branches, may read any executable byte, so every
loop in the file is left alone. That holds for a function pointer as well: a
pointer passed on or kept can be read as well as called. Only forming an
address that names a function's start (an unwind table entry), with nothing
read through it, leaves the instruction there unprotected.

`--rewrite-exclusives-mode` chooses where the rewrite applies. `overlay`
rewrites copies of the guest executable and the `--rewrite-exclusives-also`
files and bind-mounts them over the originals, which needs a mount namespace.
`memory` rewrites the loops of every executable file the guest maps, in the
guest's own pages, before any of their instructions run, and leaves the files
untouched. `auto`, the default, is `memory` under `--no-namespace` and
`overlay` otherwise. Both apply the same proof of code.

The in-memory rewrite writes into a mapping only when it can prove that the
mapped object is the file it planned from: through `/proc/<pid>/map_files`,
which needs `CAP_CHECKPOINT_RESTORE` or `CAP_SYS_ADMIN`, or by the file's
inode and device where those identify it. On btrfs or overlayfs, whose inode
numbers are not identities across subvolumes or layers, an unprivileged run
therefore writes nothing. The run summary's `exclusive_rewrites` reports each
such file as not written, with the reason, and the guest runs its original
loops. Run from a filesystem whose inode numbers identify files (ext4, xfs or
tmpfs), with the capability, or with the overlay.

### Virtual Inputs

| Option | Effect |
| --- | --- |
| `--seed=N` | Sets the fallback random seed. |
| `--rng-seed=N` | Sets only guest random-data generation. |
| `--epoch=TIMESTAMP` | Sets the virtual clock's RFC 3339 starting time. When omitted, `hermit run` and `hermit oci run` capture the host wall clock once at invocation start, print the resolved value, and use it as an immutable input for the whole run (including both `--verify` executions). Bypass modes such as `--namespace-only` and `--strace-only` neither capture nor report a virtual epoch. Pass a printed value explicitly to reproduce the same virtual-time trajectory. A guest that checks TLS certificates over the host network (`--network=host` or `--record-networking`) checks them against this clock, so an epoch outside a server certificate's validity fails the HTTPS connections to that server (curl exits 60 with "SSL certificate problem: certificate is not yet valid" or "certificate has expired"; a libcurl caller sees "SSL peer certificate or SSH remote key was not OK"). For those runs omit `--epoch`: the run then starts the clock at the current time and saves it in its manifest, and `hermit run --from last-run` repeats it with that epoch. |
| `--clock-multiplier=F` | Changes the rate of virtual time. |
| `--no-virtualize-time` | Uses host time; also requires `--no-virtualize-metadata`. |
| `--no-virtualize-cpuid` | Exposes host CPUID behavior. |
| `--cpu-profile x86-64-v3` | Gives the guest an x86-64-v3 CPU instead of the default x86-64-v2; see below. |
| `--cpu-profile host-no-rdrand` | Intercepts CPUID and returns host features with hardware randomness and unvirtualized timing features hidden; ptrace only, with CPUID faulting required. |
| `--virtual-cpus=N` | Tells the guest it has N CPUs (1 to 128) instead of one, so thread pools start N workers; see below. |

Disabling a virtualization feature introduces a host-dependent input. Do it to
diagnose compatibility problems, not when asserting full reproducibility.

#### Virtual CPUs

Without `--virtual-cpus`, a guest sees one CPU. A program that sizes a thread
pool from the CPU count (TBB, folly, `std::thread::hardware_concurrency`,
`sysconf(_SC_NPROCESSORS_ONLN)`) then starts one worker, and races between
workers cannot happen under any schedule. `--virtual-cpus=4` presents four
CPUs everywhere a program can count them: `sched_getaffinity`, `sysconf`,
`/sys/devices/system/cpu`, `/proc/cpuinfo`, `/proc/stat`, `Cpus_allowed` in
`/proc/<pid>/status`, CPUID's APIC IDs and `getcpu`. They form four
single-core sockets on one NUMA node. Hermit still runs one thread at a time,
so `--chaos` explores the pool's interleavings deterministically:

```
hermit run --chaos --sched-seed=7 --virtual-cpus=4 -- ./pool_program
```

`sched_setaffinity` then behaves as on a four-CPU Linux machine: the mask
reads back, and a mask naming none of the four CPUs is refused with `EINVAL`.
A thread's `getcpu` is a CPU of its mask chosen by the order threads were
created, and changes only when the mask leaves that CPU out. The host must
have at least N online CPUs, because the guest sees the host's `cpuN`
directories, rendered for the virtual CPUs. Only the ptrace backend (and
e9patch preprocessing on it) takes the option: other backends, `--lite` and
`hermit record` refuse it.

More CPUs can change more than the pool size. Some runtimes spin-wait
instead of sleeping when they see more than one CPU: OpenMP's spin count, Go's
scheduler, adaptive mutexes. Under serialized threads such a spin ends only at
a preemption, so a run can be slower than with one CPU.

#### The x86-64-v3 CPU profile

By default every guest sees the same x86-64-v2 CPU, so software built for
x86-64-v3 (CentOS Stream 10's glibc, for example) refuses to start.
`--cpu-profile x86-64-v3`, on `hermit run` and `hermit record start`, adds the
v3 features: AVX2, BMI1/2, FMA, F16C, LZCNT, MOVBE and XSAVE. RDRAND, RDSEED,
TSX and AVX-512 stay hidden (<https://github.com/rrnewton/hermit/issues/4243>).

With XSAVE, a guest can read processor state the host kernel saves and
restores at moments Hermit does not control. So v3 is offered only where that
is checked. `hermit host-capabilities --cpu-profile x86-64-v3` runs the check on
its own and prints QUALIFIED, or REFUSED with the reason. A v3 run is refused
by name (exit 122), and never falls back to v2:
- on any backend but ptrace;
- with `--no-virtualize-cpuid` or `--gdbserver`;
- on Linux before 5.16;
- on hybrid CPUs, on AMD CPUs without XSAVEERPTR, and on hosts with AMX;
- where the host lacks a v3 feature natively;
- where the PMU interrupt counter the check needs is unavailable (some VMs,
  or `perf_event_paranoid` above 1);
- wherever the check fails.

The guest's CPUID leaf 0xD describes the host's real XSAVE layout, because a
guest's XSAVE runs on the host. A recording therefore binds those host facts,
and `hermit replay` refuses a host where any of them differs.

What the check does not cover, which `--verify` also prints after a match under
v3:

> With --cpu-profile x86-64-v3, Hermit checks at startup that this host does not
> change the XSTATE_BV bits a guest reads (with XSAVE, XSAVEC or XSAVEOPT, or in
> a signal frame) when the host saves and restores the guest's registers. The
> check samples a fixed set of cases and forces only CPUID exits, so a CPU or
> hypervisor that behaves differently only on other instruction paths, or on VM
> exits the guest cannot see, is not excluded. Firmware (SMM) entries cannot be
> checked at all. Default runs use x86-64-v2, which this does not affect.

#### Host CPU features without hardware randomness

`hermit run --cpu-profile=host-no-rdrand` keeps CPUID intercepted, but answers
with the host CPU's features. This lets software requiring x86-64-v3 (such as
CentOS 10 glibc) use those instructions on a capable host even when the virtual
v3 profile's XSAVE qualification refuses it. For example:

```bash
hermit run --cpu-profile=host-no-rdrand --epoch=2023-01-01T00:00:00Z -- ./program
```

RDRAND (leaf 1 ECX bit 30) and RDSEED (leaf 7.0 EBX bit 18) are hidden, so
feature-detecting libraries such as libstdc++'s `std::random_device` use the
randomness Hermit virtualizes. TSX (HLE, RTM, TSXLDTRK), RDPID, WAITPKG,
MONITOR/MWAIT, MONITORX/MWAITX, RDPRU, AMD lightweight profiling, architectural
performance monitoring, VIA XSTORE randomness, and hypervisor-specific clock
interfaces are also hidden. CPU identity fields
(APIC, core and node IDs) are zeroed so tracer migration does not change them.
Before container placement, admission chooses the lowest CPU in the caller's
allowed affinity set and qualifies it. Every launch pins the actual tracer and
guest to that source CPU, including when the timeslice is disabled and when a
library reuses a tracer worker. All native leaves and subleaves therefore come
from one repeatable source: cache sharing and topology can differ between cores
even on a homogeneous CPU model. Failure to pin that source refuses the launch.
Other host features remain visible, including every XSAVE leaf 0xD subleaf;
Linux's XCOMP queries and permission requests operate on the guest's real
kernel state. RDTSC and RDTSCP still trap and return virtual time, with RDTSCP's
CPU ID zero. Leaf 0x15 reports the virtual TSC's one-billion cycles per logical
second; leaf 0x16 retains the host's static frequency metadata.

Results are **host CPU model specific**: cross-host builds need the same CPU
model, topology and compatible kernel/XSTATE settings, and repeat runs need the
same allowed CPU set so they select the same source. Intel hybrid and AMD
heterogeneous CPUs are refused because different core types can report different
features. This mode skips v3's XSTATE_BV
qualification; host save/restore activity can still affect XSAVE output and
signal-frame register metadata. CPUID masking guides programs that check
features before using them; it does not trap unconditional RDRAND/RDSEED or
the other hidden instructions. Keep input files and the environment fixed,
and verify the actual build across repeated runs.

`hermit host-capabilities --cpu-profile=host-no-rdrand` checks CPUID faulting.
A host or kernel without faulting is refused clearly, and every exec must
retain interception; the mode never falls back to native CPUID. It requires
the ptrace backend and virtual time, and conflicts with
`--no-virtualize-cpuid`, `--no-virtualize-time`, and execution modes that bypass
Detcore. Record/replay currently refuses this profile because recordings do
not bind its complete host CPU state. This includes `--record-networking`,
`--replay-networking`, and network manifests loaded with `run --from`; `replay`
refuses any manifest carrying this profile before dispatch. Use a virtual
profile for recordings. Plain history `run --from` is argument reuse, subject
to the same host requirements as a new run.

### File System, Environment, And Network

Hermit creates an isolated guest `/tmp` by default. A program built under host
`/tmp` is therefore not visible at the same path in the guest. Prefer building
outside `/tmp`, or expose only the required path:

```bash
hermit run --bind=/host/path:/guest-name -- /tmp/guest-name
```

`--bind=SOURCE` exposes a host path under guest `/tmp` with its name preserved;
`--bind=SOURCE:TARGET` selects the target under guest `/tmp` or the guest's
private `/dev/shm` (below). For general mount targets, use Docker-style
`--mount` syntax. Mount sources must exist before Hermit starts.

`--tmp=DIR` uses a chosen host directory as guest `/tmp`. `--tmp=/tmp` exposes
the real host `/tmp`, which is convenient for diagnosis but weakens isolation
and reproducibility.

Every run and recording also gets a private, empty `/dev/shm`: a fresh tmpfs,
mounted as Linux distributions mount it (mode 1777, `nosuid`, `nodev`, and
tmpfs's default size, half the host's RAM). POSIX shared memory objects and
named semaphores (`shm_open`, `sem_open`) therefore do not pass between the
guest and the host or another run. That matters because every run of a
program gets the same PIDs and random bytes: a program that names such objects
after them chooses the same names in every run, and concurrent runs sharing
the host's `/dev/shm` would open, truncate and remove each other's.

Hermit installs it after every mount whose source is a host path, so a
`--bind /dev/shm/x:/tmp/x` or a `TMPDIR` below `/dev/shm` still names the
host's, and before the binds and mounts below `/dev/shm`, which go into it. A
bind from the host's `/dev/shm` into the private one is refused, and so is
one from host `/tmp` when `TMPDIR` or `--tmp` keeps the private `/tmp` in the
host's `/dev/shm` (it is then bound first). `--bind
/dev/shm` keeps the host's `/dev/shm` for a guest that talks to host processes
through it, `--bind DIR:/dev/shm` uses host directory `DIR` instead, and a
`--mount` at `/dev/shm` (or at `/dev`) replaces it. A `--no-namespace` run uses
the host's `/dev/shm`, an `--image` run has none, and `hermit replay` does not
make one, as it answers the guest's calls from the recording.

Every run and recording also gets its own IPC namespace, empty, and a fresh
mqueue filesystem of it at `/dev/mqueue`, mounted as systemd mounts it
(`nosuid`, `nodev`, `noexec`). Detcore refuses System V IPC and the `mq_*`
calls with `ENOSYS`, as a kernel built without them would, but a guest can
still list, create, open and remove POSIX message queues as files in
`/dev/mqueue`: those are now its own, not the host's or another run's. A
`--namespace-only` guest, whose System V and `mq_*` calls reach the kernel,
likewise gets keys and names that no other run or host process holds; it no
longer reaches the host's System V objects or message queues, which only a
`--no-namespace` run, sharing the host's IPC namespace, still does. A
`--mount` at `/dev/mqueue`, `/dev` or `/` replaces the private mqueue mount,
and `--mount type=bind,source=/dev/mqueue,target=/dev/mqueue` shows the
host's queues there; a mount below `/dev/mqueue` is refused. `hermit replay`
gets the IPC namespace but no private `/dev/mqueue`, and an `--image` run's
minimal `/dev` has none.

DBT reserves `/tmp/.hermit-dbt-stats-N` for its current run's statistics, choosing
the lowest number that does not overlap a declared bind target. The backing
capture remains private to that run. With `--tmp=DIR`, this can create an empty
directory or temporarily cover an existing directory at that name; existing
contents are preserved. An incompatible file at the target is a mount error.
`--no-namespace` keeps the host capture path and adds no statistics bind.

The guest's mount table (`/proc/<pid>/mountinfo`, `/proc/<pid>/mounts` and
`/proc/mounts`) is a fixed description taken when the guest launches, not the
host's live table. It is the same on every backend, and in every run as long as
the mounts it lists (and, under `--no-namespace`, the mounts holding the working
directory and `/tmp`) do not change on the host between runs. In Hermit's
container it lists the mounts Hermit installed (its `/proc`, `/tmp`,
`/dev/shm`, `/dev/mqueue`, identity files and your `--mount` and `--bind`
targets), the ancestors of those mounts, and the base mountpoints `/`, `/proc`,
`/dev`, `/dev/pts`, `/dev/shm`, `/dev/mqueue`, `/sys`, `/sys/fs/cgroup` and
`/run` where
they are mounted. A filesystem Hermit or a `--mount` mounts afresh in the
container (not a bind) shows no `uid=` or `gid=` option for the host user who
ran Hermit, as the same mount made by root shows none; a `--namespace-only`
guest, which reads the kernel's table, still sees them. With
`--no-namespace`, where Hermit installs no mount, it lists the mounts holding
the guest's working directory and the host `/tmp`, their ancestors, and those
base mountpoints. Every other host mount is hidden, although its files stay
reachable: a file on a hidden mount reports its nearest listed ancestor in
`/proc/self/fdinfo` (`mnt_id`) and `statx` (`STATX_MNT_ID`), and `df` on it
prints `-` as the source and that ancestor's mountpoint. This keeps unrelated
host mount activity out of the guest's view, at the cost of a less complete
table than Linux shows.

When a runtime supplies authenticated private-memory intervals, the guest's
`maps` and `smaps` views exclude those intervals using the opened target's
ownership, including inherited and duplicated descriptors. `smaps` removes
complete private VMA sections before assigning mapping identities. A VMA that
mixes private and guest memory refuses with `runtime-private smaps view
unavailable: mixed ownership has unsupported per-VMA accounting`: its aggregate
accounting cannot faithfully describe separate guest fragments. `maps` retains
its existing fragment support. Guest sections keep the existing Smaps sanitizer
policy; its fallback for unknown fields or malformed values can retain raw
device, inode and accounting fields in the projected guest view. Ambiguous
section framing refuses instead of hiding a possible guest section.

This projection does not cover `smaps_rollup`, `numa_maps` or `map_files`, or
runtime mappings absent from the supplied ownership facts.

Network modes are:

- `--network=local` (default): isolated loopback networking. Each run has its
  own network namespace, so its ports and abstract unix socket names are its
  own.
- `--network=host`: host networking. External responses and port state become
  nondeterministic inputs, and the guest shares the host's ports and abstract
  unix socket names with every other program, other Hermit runs included.

Prefer `--network=local` unless the guest must reach the host's network. An
abstract unix socket name (`@name`, a `sun_path` that starts with a NUL byte)
belongs to the network namespace, not the file system. Every Hermit guest's
first process is pid 3, and Hermit picks autobind names (`@` and five hex
digits) the same way in every run. So two concurrent `--network=host` runs
whose guests bind the same abstract name, for instance one built from the pid,
collide: the second bind fails with `EADDRINUSE`, although natively each
process would have used its own name. On the ptrace backend, when a
`--network=host` guest binds an abstract name, or fails to with `EADDRINUSE`,
Hermit prints one notice per run on stderr:

```text
hermit: NOTICE: the guest bound the abstract unix socket "@myapp:3" under --network=host. Abstract names belong to the network namespace, ...
```

The bind itself is unchanged. Run such guests with `--network=local`, or one at
a time when they need the host's network. `--strace-only` includes
`--network=host`; to give such a run its own names, spell out its expansion
with `--network=local` instead.

`hermit run --gdbserver` serves the guest to one gdb client and waits for it
before the guest runs; only the ptrace backend (the default) serves gdb, and
the other backends refuse the flag. Attach from another terminal with
`gdb -ex 'target remote 127.0.0.1:PORT' PROGRAM` (default port 1234; override
it with `--gdbserver-port=PORT`, or pass `0` for any free port, which the run
prints). Hermit binds that port in the host's network namespace before it
makes the guest's, so the guest keeps the network `--network` gives it,
`--analyze-networking` included. A `--strict` run can be debugged with every
strict refusal in force:

```bash
hermit run --strict --epoch=2026-01-01T00:00:00Z --gdbserver -- PROGRAM
```

The debugger is an input of the run that `--strict` cannot control.
Breakpoints write `int3` into the guest's code, and writes to its memory or
registers, the signals gdb sends and the moment it interrupts the guest all
change the run; another session repeats the run only if it does the same.
With a gdbserver, Detcore also takes no reports of the guest's signal-handler
returns (docs/SIGNAL_STATE_CHECK.md), as in a recording and its replays: after
a handler that took a scheduler turn, another thread can run before the next
signal is delivered, so a multithreaded guest can be scheduled differently
than without a debugger. A replay served to gdb (`hermit replay`,
`--serve-only`, `hermit record start --verify-with-gdbex`) binds its port the
same way and keeps the network the recording used.

Environment controls include `--base-env=empty`, `--base-env=minimal`,
`--base-env=host`, `-e NAME[=VALUE]`, and `--workdir=PATH`.

`--base-env=minimal` gives the guest exactly these variables, with the same
values on every host, plus the `ASAN_OPTIONS` and `LSAN_OPTIONS` leak-check
overrides every instrumented guest gets:

| Variable | Value |
| --- | --- |
| `HOSTNAME` | `hermetic-container.local` |
| `PATH` | `/usr/local/sbin:/usr/local/bin:/usr/sbin:/usr/bin:/sbin:/bin` |
| `HOME` | `/root` |
| `USER` | `root` |
| `LOGNAME` | `root` |

`HOME`, `USER` and `LOGNAME` name the guest's own identity, as login sets them
for a user: Hermit presents every guest user ID 0 (`getuid()` returns 0), and
the guest's user database (`getpwuid(0)`) names that user `root` with home
directory `/root`. A program that reads `USER` sees the same user as one that
asks the user database. `/root` is usually the host's root home directory, which
the guest can find but usually cannot list or write. Give a program that writes
under its home directory a writable one, for example `-e HOME=/tmp`.
`-e NAME=VALUE` replaces any of these values, and `--base-env=empty` sets none
of them. `LANG`, `TERM`, `TZ` and `SHELL` are not set; pass them with `-e` when
a program needs them.

## Logs And Run Summaries

Global logging options go before the subcommand:

```bash
hermit --log=info --log-file=hermit.log run --summary \
  --summary-json=summary.json -- PROGRAM
```

The equivalent environment variables are `HERMIT_LOG` and
`HERMIT_LOG_FILE`. Use `debug` or `trace` only for focused diagnosis; output can
be large.

The JSON summary also holds `host_inputs`, the host files the guest read
(see the replay paragraph under "Chaos Mode"), with a `digest` of the set,
and on a schedule replay `host_inputs_changed`, the files that changed since
the recording. Two separate runs of the same command whose digests differ
read different host files (or, for a file that was not hashed, the same path
at another inode, size or time), so a difference in their results may come
from those files rather than from Hermit: compare the `files` to find which,
and rerun with them pinned before calling it a Hermit divergence.

For a minimally invasive interception trace:

```bash
hermit --log=info run --strace-only -- PROGRAM
```

`--strace-only` does not determinize execution. It exposes host `/tmp` and
networking and disables Hermit's virtualized inputs, sequential scheduling,
deterministic I/O, and RCB time. It is a compatibility diagnostic, not a run
mode for reproducibility.

`--namespace-only` runs the command in Hermit's namespaces without ptrace or
determinization. It helps separate namespace setup failures from interception
failures. One call is still watched, through a seccomp filter on `kill`: the
guest shares Hermit's process group, so Hermit refuses with EPERM a
`kill(0, sig)` from a guest process still in that group. Linux would deliver
it to Hermit, to the processes that started it, and to the guest's own group
members; none of them receives it, so a script ending with `trap "kill 0" EXIT`
leaves its background children running until the guest exits. A guest process
that first leads a group of its own (`setsid`, `setpgid(0, 0)`) keeps its
`kill(0)`. The filter is visible inside the guest: `/proc/self/status` reports
`Seccomp: 2`, a seccomp listener of the guest's own fails with EBUSY, and a
Hermit run inside the guest refuses the inherited filter (exit 122) unless
given `--unsafe-ignore-host-seccomp`; a nested `--namespace-only` run still
refuses (exit 122), because its own filter cannot add a second listener.

## Run Configuration Files

`hermit run --save-config=run.yaml ...` writes the run's options to a file, and
`hermit run --config=run.yaml` runs with them again:

```bash
hermit --log=info run --seed-from=SystemRandom --save-config=run.yaml -- PROGRAM ARGS
hermit run --config=run.yaml
```

A run config is YAML (JSON is accepted, being YAML) that names options by their
long names, without the leading `--`:

```yaml
schema: hermit-run-config/v1
hermit-version: 0.4.1 (2026-10-08, source revision not embedded)
global:            # options before the subcommand: hermit [GLOBAL] run
  backend: ptrace
  log: info
run:               # options of `hermit run`
  base-env: minimal
  epoch: 2026-10-08T17:54:04.637477466+00:00
  seed: 12919933106523941100
  verify: true     # a flag: true gives it; false is the same as leaving it out
  env:             # a repeatable option: one list entry per occurrence
  - FOO=1
  - BAR=2
program: /bin/sh
args: [-c, echo $FOO$BAR]
```

- A file means exactly what typing its options would: Hermit turns each entry
  into the option it names and checks the result like any command line, so
  every `hermit run` option can appear and the same combinations are refused.
- An option given on the command line replaces the file's value for that
  option (for a repeatable option, its whole list). A guest program on the
  command line replaces the file's `program` and `args`. The file's values
  replace the options' environment variables (`HERMIT_EPOCH`, `HERMIT_LOG`, ...).
  A flag the file gives cannot be taken back on the command line, because
  flags have no negative spelling; edit the file instead.
- `--save-config` writes the options given on the command line, in a `--config`
  file, or through their environment variables, and the implicit inputs Hermit
  resolved: the epoch taken from the host clock (or from a replayed recording)
  and the seed `--seed-from` chose. It is written on the host before the guest
  starts. Defaults are not written, so a file reproduces its run under the
  Hermit version recorded in it. The host environment that `--base-env=host`
  passes through is not recorded; use `--base-env=minimal` and `-e` to make the
  guest environment part of the file.
- `schema` is required and must be `hermit-run-config/v1`. Unknown keys are
  refused, with the spelling that would work where there is one. The top-level
  key `seccomp` is reserved for a future syscall policy section and is refused
  until then. `--config` and `--save-config` cannot appear in a file.
- `hermit oci run`, `hermit analyze` and `hermit bisect` do not load run
  configs.

## Run Manifests

A run whose options give every input is reproduced by its command line. A run
that draws an input from the host is not: an epoch taken from the host clock
(no `--epoch`), or a seed from `--seed-from=SystemRandom`. Such a run writes a
run manifest, a small recording of those inputs, and names it in a warning
(https://github.com/rrnewton/hermit/issues/4271):

```text
$ hermit run --seed-from=SystemRandom -- PROGRAM ARGS
... WARN hermit::controller: this run drew settings from the host; its inputs are saved as a recording in ~/.cache/hermit/; rerun it with: hermit replay 04cba9fc... (hermit show last-run)
$ hermit replay last-run             # rerun the newest run from its manifest
$ hermit run --from last-run --sched-seed=7   # the same inputs, another schedule
$ hermit list
ID        KIND          CREATED               EXIT        COMMAND
04cba9fc  inputs-only   2026-10-13T19:55:00Z  0           PROGRAM ARGS
```

- The manifest is `DATA/<id>/manifest.json`, where `DATA` is the data
  directory `hermit record` uses (`HERMIT_DATA_DIR`, else
  `$XDG_CACHE_HOME/hermit`, usually `~/.cache/hermit`) and `<id>` a new
  recording ID. It is JSON with `schema: hermit-recording-manifest/v1`: the
  guest command, the run's options with the host-drawn inputs written out
  (`inputs.global` and `inputs.run`, named as in a run config), which inputs
  came from the host (`inputs.host_drawn`), and, once the guest ends, its exit
  status (`exit`).
- What a manifest does not hold, as a run config does not: the host
  environment that `--base-env=host` (the default) passes through, the files
  the guest reads, and the working directory, which is recorded in
  `command.cwd` but not applied (relative paths resolve against the rerun's
  directory, and `--from` notes a difference). Output destinations
  (`--log-file`, `--verify-json`, `--verify-run`, ...) are left out, so a
  rerun does not overwrite the original run's files. On a host without a
  usable performance counter, the preemption timer turned off is recorded
  as `max-timeslice: disabled`. Option values are written as given, `--env`
  values included; the file is mode 0600.
- Every kind of recording has one name, which `hermit replay`,
  `hermit run --from` and `hermit show` take. It is one of:
  - `last` (`DATA/last`): the newest recording you asked for, by
    `hermit record` or `hermit run --record-networking`. A bare
    `hermit replay` replays it, as `rr replay` replays the newest
    `rr record`.
  - `last-run` (`DATA/last-run`): the newest of any kind, including the
    manifest a plain run leaves. A run without `--epoch` draws its epoch from
    the host and so leaves a manifest. It moves only `last-run`, so it never
    takes a bare `hermit replay` away from your recording.
  - a recording ID, or a unique prefix of at least 6 hex digits, as git
    takes a commit;
  - the path of a `manifest.json` or of its directory. An existing path is
    read as a path before an ID; write `./last` for a file called `last`.
- `hermit replay [RECORDING]` replays any kind, by default `last`. A syscall
  recording replays as before, under gdb or to the end with `--autopilot`.
  Any other kind replays by rerunning its program from its manifest, to the
  end, exactly as `hermit run --from RECORDING`, with the global options
  given before `replay` passed on. Such a rerun is not served to gdb:
  `hermit run --from RECORDING --gdbserver` does that, and `--serve-only`,
  `--gdbex` and `--dap-listen` with such a recording are refused naming it.
- `hermit run --from RECORDING` reruns it. Options and a guest command on
  that command line replace the manifest's, as with `--config`, so
  `hermit run --from last-run --sched-seed=7` explores another schedule from
  the same inputs. A rerun's inputs are all explicit, so it writes no manifest of
  its own.
- `hermit list` lists the recordings of every kind, newest first (`--json`
  for the full IDs, kinds, times, exit statuses, commands and paths), and
  `hermit show [RECORDING]` prints one's manifest.
- A run with no host-drawn input writes nothing. `--manifest=POLICY` chooses
  otherwise: `nonreproducible-only` (the default), `always` (a reproducible run
  also writes a `history` manifest, a local record of what ran), or `never`.
  Without the option, the `manifest` key of the user config file
  `~/.config/hermit/config.yaml` (`$XDG_CONFIG_HOME/hermit/config.yaml`)
  decides, then the `HERMIT_MANIFEST` environment variable:

  ```yaml
  manifest: always
  ```

- `hermit record list` lists syscall recordings only. `hermit record rm` and
  `hermit record clean` delete manifest directories as they delete
  recordings. A data directory keeps the newest 1000 manifest-only
  recordings.
- A network recording, `hermit run --record-networking=TRACE`, always writes
  a manifest beside its trace, `TRACE.manifest.json` (kind `network`): the
  run's options with any host-drawn epoch or seed written out, and the
  trace's format version, size and SHA-256. The trace itself does not hold
  the seeds or the command, so `hermit run --from TRACE.manifest.json`
  replays the recording offline without retyping them; a trace that does not
  match the manifest's size and digest is refused. The data directory keeps
  a copy of that manifest naming the trace by its absolute path, so the
  recording also has an ID and becomes `last`: `hermit replay` alone replays
  it offline. A pruned copy takes that ID with it; `hermit run --from
  TRACE.manifest.json` still replays the recording. Those copies are pruned with the manifest-only recordings; the
  trace and the manifest beside it never are.
- A syscall recording, `hermit record`, writes `manifest.json` (kind
  `full-syscall`) beside its `metadata.json`: the guest command, how it
  ended, and the recording's `RECORD_VERSION`. The replayer still reads
  `metadata.json`, which stays as a compatibility copy for one
  `RECORD_VERSION`; `--from` refuses such a manifest and names the
  `hermit replay` command instead.

## Signals To Hermit's Own Process

In a run with interception (the default ptrace backend), Hermit's container
process is PID 1 of the guest's PID namespace, the guest's parent. To the
guest it is the namespace's init, and Hermit keeps it out of the guest's
reach (https://github.com/rrnewton/hermit/issues/4066,
https://github.com/rrnewton/hermit/issues/4083):

- A signal the guest sends to PID 1 or one of its threads (`kill`, `tkill`,
  `tgkill`, `rt_sigqueueinfo`, `rt_tgsigqueueinfo`), SIGKILL included, is
  dropped and the call returns 0, as Linux does for a signal sent from inside
  a PID namespace to its init when the init has no handler. Signal 0 still
  answers whether the task exists.
- `pidfd_open(1)` fails with EPERM, so no pidfd reaches Hermit. Linux would
  open it.
- Making one of Hermit's tasks the owner of a descriptor's signals fails
  with EPERM: `fcntl` `F_SETOWN` or `F_SETOWN_EX`, and `ioctl` `FIOSETOWN` or
  `SIOCSPGRP`. Linux would accept the owner and later signal it when the
  descriptor is ready.

A guest in a nested PID namespace (created with `clone(CLONE_NEWPID)`) is
unaffected: its ids name its own tasks. The EPERM answers are a deliberate
policy, the same answer the guest already gets for `ptrace` and
`process_vm_readv` on that process.

## Inherited Seccomp Filters

A seccomp filter installed by whatever launched Hermit, such as a container
runtime's default profile, is inherited by Hermit and every guest, and cannot be
removed. It can make system calls fail in ways the guest observes, and Hermit
records nothing about it, so the same command could behave differently inside
and outside the container. Hermit therefore refuses to start a guest (exit 122)
when `/proc/self/status` reports `Seccomp: 1` or `Seccomp: 2`. The refusal
names the mode and the filter count.

- Run Hermit where no filter is inherited. A filter cannot be removed from
  inside a running container; for Docker, create the container with
  `--security-opt seccomp=unconfined`.
- Or pass `--unsafe-ignore-host-seccomp` (before or after the subcommand; in a
  run config, `global: {unsafe-ignore-host-seccomp: true}`) to run anyway. The
  run may not reproduce wherever the filter differs. Hermit prints a warning
  and records the filter's mode and count in the INFO log.

Declaring an acknowledged host filter in the run config is tracked in
<https://github.com/rrnewton/hermit/issues/3942> and not implemented.

## Troubleshooting

### Program Not Found Or Not Executable

Hermit resolves bare program names through the guest `PATH`. Check the path,
execute bits, shebang interpreter, environment, and mounts:

```bash
command -v PROGRAM
ls -l /absolute/path/to/PROGRAM
hermit run --base-env=host -- /absolute/path/to/PROGRAM
```

If the program is under host `/tmp`, either move it, use `--tmp=/tmp`, or bind
it into guest `/tmp`. Hermit's startup error names this case explicitly.

### Namespace Setup Fails With EPERM

Hermit needs unprivileged user, PID, IPC, and mount namespace support. Container
runtimes and hardened hosts may disable these facilities. A useful host probe
is:

```bash
unshare --user --map-root-user --pid --ipc --mount --fork true
```

If that fails, adjust the host or container policy. Running Hermit as root is
not a general substitute for the required namespace support.

### Ptrace Or Seccomp Is Denied

Hermit performs explicit startup probes and reports whether parent-child
`PTRACE_TRACEME` or `seccomp(SECCOMP_SET_MODE_FILTER)` was denied.

- Allow same-UID parent-child ptrace in the container seccomp profile and host
  Yama/LSM policy. `CAP_SYS_PTRACE` is normally not required for this relation.
- Allow seccomp filters and `prctl(PR_SET_NO_NEW_PRIVS)`.
- Try `hermit run --namespace-only -- /bin/true`. If it works, namespaces are
  available and the failure is in interception policy.

Inspect host policy without changing it blindly:

```bash
cat /proc/sys/kernel/yama/ptrace_scope 2>/dev/null || true
```

### Performance Counters Are Unavailable

Hermit prints a warning and continues with `--max-timeslice=disabled`
when `perf_event_open` is unavailable. Check:

```bash
cat /proc/sys/kernel/perf_event_paranoid
```

The host setting, VM PMU exposure, and container seccomp policy can all block
performance counters. Enable them when precise scheduling preemption matters.
Otherwise pass `--max-timeslice=disabled` explicitly and understand that
CPU-bound threads may run until another intercepted event.

### Unsupported System Calls

Hermit implements deterministic behavior for many, but not all, Linux system
calls. By default, a call classified as Unsupported is rejected instead of
being forwarded to Linux. The run-only `--allow-unsupported-syscalls` option
permits pass-through and prints a warning; it may restore compatibility but
introduces uncontrolled behavior. A successful exit with this option does not
establish reproducibility.

To identify the first unsupported call during development:

```bash
hermit --log=info run --panic-on-unsupported-syscalls -- PROGRAM
```

This option intentionally panics and is a diagnostic, not a production mode.
Compare with `--strace-only` to determine whether interception or Hermit's
deterministic model causes the failure. Reduce the command to a small reproducer
before reporting an issue.

### Android Binder

A binder call that waits for another process (a blocking
`ioctl(BINDER_WRITE_READ)` read) cannot hold the guest's
turn, so Hermit runs it off the turn, and the call then finishes when the host's
kernel delivers its work. A waiting `BINDER_FREEZE` is host-timed, so it is
refused by name instead (the same refusal returns `EOPNOTSUPP` under
`--allow-unsupported-syscalls`); it never runs off the turn. Hermit does that only while the calling task is the
run's one task: no other thread or process of the guest is live, exited but not
yet reaped, or being created, and no debugger is attached (`--gdbserver`,
`--dap-listen`). Then no other task can change what the call reads and writes,
use a descriptor it delivers before Hermit records it, or take the work it waits
for. A single-threaded binder client or service, whose peer runs outside Hermit,
works this way. A thread that ends by its own `exit` without leading its
process stops counting when Hermit commits that exit in the thread's turn, after
which it never runs again, so a program that joined its helper threads and runs
on alone is alone again (on the ptrace backend; elsewhere, or with
`--no-sequentialize-threads`, it counts until its process is reaped). Every
other ended task counts until a wait reaps its process: a process that exited, a
thread-group leader that exited while its threads ran on, and a thread that an
exec, an `exit_group` or a signal ended. A child reaped without a wait (its
parent ignores `SIGCHLD` or set `SA_NOCLDWAIT`), and an orphan reaped by a
process outside the guest, are never removed. Once such a task exists, the run
is never alone again, so a fail-closed run refuses its waiting binder calls.

Beside any other task, such a call is refused by name, naming the other tasks,
and the run ends with the policy-refusal status (122). With
`--allow-unsupported-syscalls` the same named refusal returns `EOPNOTSUPP` to
the guest instead of ending the run (`HERMIT_SHARED_BINDER_CALL`); the call is
not issued either way: a libbinder thread pool, or a client and service that
both run under Hermit and wait for each other, cannot be served this way. A
call that sends and waits in one `BINDER_WRITE_READ` (as libbinder's
`talkWithDriver` does) is refused the same way, recorded as
`HERMIT_HOST_TIMED_BINDER`. Calls that do not wait, such as every read on a
descriptor opened `O_NONBLOCK`, run in the caller's turn as usual.

A `--strict` run never holds a binder descriptor, in a fresh run and in a
replay alike: binder reply payloads are not recorded, so no run can repeat
what a binder call returns (https://github.com/rrnewton/hermit/issues/5124).
The run ends by name, with the policy-refusal status (122), where a binder
descriptor would reach the guest: an open of a binder device, a descriptor
received through `SCM_RIGHTS` or `pidfd_getfd`, or one the run starts with,
such as a binder device passed as standard input. Hermit tells a binder
device by the open file (a character device on binderfs, or the misc device
binder registers without binderfs), never by its path, and through the
calling thread's own descriptor table. A descriptor whose identity Hermit
cannot read is refused the same way. That includes a misc device with a
dynamically assigned minor on a kernel where no binder driver's `devices`
parameter can be read (`binder` or `binder_linux`), such as
`/dev/userfaultfd` on a kernel without binder. Other character devices, such
as `/dev/null`, `/dev/urandom`, terminals, `/dev/fuse` or `/dev/net/tun`, are
not binder devices. As a backstop, a binder `ioctl` request on a descriptor
Hermit cannot tell is not a binder device is refused too, before Hermit waits
for anything or reads the call's argument. Under the KVM backend, guest
descriptors are not host descriptors Hermit can inspect
(https://github.com/rrnewton/hermit/issues/5129). So, as a temporary
conservative policy, a `--strict` run is refused before it starts on a host
that offers binder in any form, whether or not the guest would use it. That
means binderfs listed in `/proc/filesystems`, a misc device in `/proc/misc`
whose name contains "binder" or that the binder driver's `devices` parameter
names, or a binder descriptor among those Hermit's guest inherits. It is
refused, too, when any of that, or that parameter, cannot be read, when
`/sys/module` is not visible as sysfs, where a missing parameter would not
show that there is no binder driver, or when `/sys/module` lists the binder
module without its `devices` parameter. The check trusts these host files as
the kernel presents them; a host that deliberately mounts a substitute over
them is outside `--strict`'s threat model. Run a guest that uses binder
without `--strict`.

### The Guest Hangs

1. Run the command normally to confirm the program itself terminates.
2. Try `--namespace-only` to test namespace setup without interception.
3. Try `--strace-only` with `--log=info` to test basic interception.
4. Try `--no-sequentialize-threads --no-deterministic-io` to isolate scheduler
   and I/O modeling. This weakens determinism and is diagnostic only.
5. Check whether PMU preemption was disabled. CPU-bound or `sched_yield` loops
   can starve without timer preemption.
6. Capture a log and the exact command line before terminating the run.

For programs that intentionally leave background processes, `--kill-daemons`
can terminate remaining tasks once only daemons remain.

### Verification Differs Between Runs

Look for inputs outside Hermit's model:

- files modified by the first run;
- host environment variables (`--base-env=host` is the default);
- host networking or external services;
- shared memory, devices, or other host processes;
- unsupported system calls passed through to Linux;
- explicitly disabled virtualization or thread serialization.

Start with `--base-env=minimal`, isolated networking, immutable files, and a
fresh work directory. Save logs from both runs when narrowing the difference.

### Record Or Replay Fails

- Use `hermit record list` to confirm the ID and data directory.
- Pass the same `--data-dir` or `HERMIT_DATA_DIR` to record, list, and replay.
- Use `--autopilot` when GDB is not installed or interactive debugging is not
  intended.
- Keep the captured directory unchanged and use a compatible Hermit revision.
- Record/replay support is experimental; some system calls and ancillary file
  descriptor behavior remain incomplete.
- Replay maps recorded copies of file contents. Every `MAP_SHARED` mapping of
  one file, in any process and PID namespace, maps one replay copy of that file
  at its offset, opened with the access mode of the descriptor it was mapped
  through, so a store through one is read through the others, as under Linux,
  and a mapping of a descriptor opened read-only cannot be made writable. A
  write through a descriptor reaches the mappings already made only when
  replay holds the file itself (a file the recording created, or a memfd);
  otherwise it reaches only mappings made after it, whose recorded contents
  include it.

### CPUID Or Hardware Warnings

CPUID faulting and PMU behavior differ across CPUs, VMs, and container hosts.
Record the CPU model and virtualization environment when reporting a failure.
`--no-virtualize-cpuid` may help diagnosis, but makes the guest depend on host
CPU features.

Some KVM guests with an AMD vCPU model on Linux 6.16 or older, such as
on-demand cloud VMs, report `ptrace-cpuid-faulting-unavailable ... errno=19`
or `CPUID faulting is unavailable`. Their hypervisor supports CPUID faulting,
but the kernel does not detect it. On such a guest, run
`tools/cpuid-fault-kmod/enable-cpuid-faulting.sh` from the Hermit source tree
once per boot, before reproducible builds. It loads a small kernel module that
turns on Linux's own per-task CPUID faulting. Where faulting already works, it
changes nothing. See [tools/cpuid-fault-kmod/README.md](../tools/cpuid-fault-kmod/README.md).
On such a guest, Hermit adds a `note:` with this pointer to the error.

## How Hermit Works

Hermit has three main layers:

1. **Hermit CLI and container setup.** The CLI creates Linux namespaces,
   mounts `/proc`, maps the caller inside the namespace, prepares the guest
   environment and mounts, and starts the command.
2. **Reverie instrumentation.** Reverie uses ptrace and seccomp-assisted event
   interception to observe guest system calls and events such as CPUID, RDTSC,
   signals, and timer preemptions. It can suppress a call, inject another call,
   or forward work to Linux.
3. **Detcore determinization.** Detcore maintains per-thread and global state,
   supplies virtual time and random values, sanitizes kernel results, models
   shared resources, and runs the deterministic scheduler.

Strict scheduling serializes guest threads so only one makes progress at a
time. Detcore counts retired conditional branches with the CPU PMU and uses a
timer to end a time slice. At each scheduling point it chooses the next runnable
thread deterministically; chaos mode makes that choice with a seeded PRNG.

Some calls are fully emulated, some are forwarded and sanitized, and calls
classified Unsupported are rejected unless the run-only
`--allow-unsupported-syscalls` option lets them reach Linux. This is why Hermit
can run ordinary unmodified binaries but cannot guarantee determinism for every
Linux interface.

## Reproducibility Checklist

Before treating a result as reproducible:

- Use the same Hermit revision, executable, arguments, and configuration.
- Save the run's options with `--save-config` and rerun with `--config`; the
  file records the epoch and any seed Hermit chose.
- Keep file contents and mount layout fixed. On a host whose packages
  change, that includes system files such as `/etc/ld.so.cache`; compare the
  runs' `--summary-json` `host_inputs` digests, or pin a file with
  `--mount type=bind,source=COPY,target=/etc/ld.so.cache,readonly`.
- Prefer `--base-env=minimal` and explicit `-e` variables.
- Keep networking isolated; do not depend on external services.
- Keep strict thread serialization and deterministic I/O enabled.
- Record all seeds used by chaos mode.
- Confirm PMU preemption is available when exploring CPU-bound schedules.
- Run once with `--verify` when the workload is idempotent.
- Investigate unsupported syscall passthroughs for the target workload.

## Further Reference

- `hermit --help`
- `hermit run --help`
- `hermit record --help`
- `hermit replay --help`
- `hermit analyze --help`
- [`README.md`](../README.md)
- [`docs/Developers/Architecture.md`](Developers/Architecture.md)
- [Issue tracker](https://github.com/rrnewton/hermit/issues)
