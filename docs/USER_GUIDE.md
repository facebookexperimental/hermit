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

## Supported Environment

Hermit supports x86_64 Linux. It requires:

- A Rust nightly toolchain. The repository's `rust-toolchain.toml` selects it
  automatically when Rust is managed by `rustup`.
- Linux user, PID, and mount namespaces.
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
`--strict` option remains for command-line compatibility but does not make the
default stricter.

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
- gives the guest an isolated PID namespace and `/tmp`;
- uses an isolated local network namespace;
- exposes most of the host file system read/write.

For a cleaner starting environment, use a minimal environment and pass only
the variables the program needs:

```bash
hermit run --base-env=minimal -e LANG=C --workdir=/tmp -- /bin/pwd
```

#### Backend Selection

Use `--backend=ptrace|dbt|liteinst|sabre|kvm|e9patch` to select the process
instrumentation backend.
It is a global option and belongs before the subcommand, but backend scope is
command-specific. Every backend supports `run`; KVM also supports `analyze` and
`bisect`, which launch `run` trials (`hermit --backend=kvm analyze ...`);
e9patch also supports `record`, and SaBRe also supports `strace`; unsupported
combinations fail closed. The default is
`ptrace`, so existing commands are unchanged:

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

Build the runtime beside a Hermit built with the optional `liteinst` feature,
which `third-party-backends` includes; a build without it refuses
`--backend=liteinst`:

```bash
cargo build --locked -p hermit --features liteinst --bin hermit
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

The `dbt`, `sabre`, and `e9patch` selections require optional CLI features as
well as their runtime resources. Build and stage both with:

```bash
cargo build --release --workspace --features hermit/third-party-backends
./target/install_pkg/hermit --backend=dbt run -- /bin/echo hello
```

For DBT, the default `--network=local` gives each physical execution a fresh
network namespace with active loopback, including both executions of
`--verify`. Each guest and its verification channel share that namespace.
Newly created guest sockets are isolated from unrelated outer listeners; a
namespace setup failure stops before the guest runs. Explicit `--network=host`
shares the caller's network and remains incompatible with `--strict`.

The same executable accepts `--backend=sabre` and `--backend=e9patch`.
A default `cargo install` does not enable these selections. KVM requires
read-write `/dev/kvm` access plus a guest-kernel ABI.

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
`--target-timeslice=N` adds a cheaper logical deadline checked at syscall
boundaries; it is useful for workloads that enter the kernel frequently.
`--max-timeslice=disabled` avoids PMU use but can miss bugs in
CPU-bound code that rarely makes system calls. The old
`--preemption-timeout` spelling is a deprecated alias for
`--max-timeslice`.

Advanced investigations can save preemption decisions with
`--record-preemptions-to=FILE` and replay them with
`--replay-preemptions-from=FILE`.

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

## Common Run Options

### Scheduling And Determinism

| Option | Effect |
| --- | --- |
| `--strict` | Compatibility spelling for the current deterministic defaults. |
| `--allow-unsupported-syscalls` | Run-only compatibility escape hatch that permits Unsupported syscalls to reach Linux and prints a warning. A successful exit does not establish complete deterministic execution. |
| `--passthru-opt` | Use the reduced syscall subscription set for performance. Requires `--allow-unsupported-syscalls`; unlisted PassThrough syscalls bypass Detcore, weakening deterministic accounting. Unsupported syscalls remain subscribed so the explicit policy is observable. |
| `--no-sequentialize-threads` | Lets Linux schedule guest threads concurrently. This weakens schedule reproducibility. |
| `--no-deterministic-io` | Disables Hermit's deterministic short-I/O completion behavior. |
| `--chaos` | Uses seeded randomized deterministic scheduling. |
| `--sched-seed=N` | Selects a reproducible chaos schedule. |
| `--target-timeslice=N` | Ends a logical turn at the first syscall boundary after N virtual nanoseconds. |
| `--max-timeslice=N` | Sets the maximum virtual time slice (minimum one scaled RCB) and requires PMU support. |
| `--max-timeslice=disabled` | Disables PMU timer preemption. |
| `--preemption-timeout=N` | Deprecated alias for `--max-timeslice=N`. |

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

### Virtual Inputs

| Option | Effect |
| --- | --- |
| `--seed=N` | Sets the fallback random seed. |
| `--rng-seed=N` | Sets only guest random-data generation. |
| `--epoch=TIMESTAMP` | Sets the virtual clock's RFC 3339 starting time. When omitted, `hermit run` and `hermit oci run` capture the host wall clock once at invocation start, print the resolved value, and use it as an immutable input for the whole run (including both `--verify` executions). Bypass modes such as `--namespace-only` and `--strace-only` neither capture nor report a virtual epoch. Pass a printed value explicitly to reproduce the same virtual-time trajectory. |
| `--clock-multiplier=F` | Changes the rate of virtual time. |
| `--no-virtualize-time` | Uses host time; also requires `--no-virtualize-metadata`. |
| `--no-virtualize-cpuid` | Exposes host CPUID behavior. |

Disabling a virtualization feature introduces a host-dependent input. Do it to
diagnose compatibility problems, not when asserting full reproducibility.

### File System, Environment, And Network

Hermit creates an isolated guest `/tmp` by default. A program built under host
`/tmp` is therefore not visible at the same path in the guest. Prefer building
outside `/tmp`, or expose only the required path:

```bash
hermit run --bind=/host/path:/guest-name -- /tmp/guest-name
```

`--bind=SOURCE` exposes a host path under guest `/tmp` with its name preserved;
`--bind=SOURCE:TARGET` selects the target under guest `/tmp`. For general mount
targets, use Docker-style `--mount` syntax. Mount sources must exist before
Hermit starts.

`--tmp=DIR` uses a chosen host directory as guest `/tmp`. `--tmp=/tmp` exposes
the real host `/tmp`, which is convenient for diagnosis but weakens isolation
and reproducibility.

Network modes are:

- `--network=local` (default): isolated loopback networking.
- `--network=host`: host networking. External responses and port state become
  nondeterministic inputs.

`hermit run --gdbserver` needs a host gdb client to reach the gdbserver port.
Because `--network=local` binds that port inside the guest's isolated network
namespace, run mode forces host networking (printing a warning) whenever
`--gdbserver` is set so the debugger can attach. Attach from another terminal
with `gdb -ex 'target remote :PORT'` (default port 1234, override with
`--gdbserver-port=PORT`). `--gdbserver` cannot be combined with
`--analyze-networking`, which requires the isolated namespace.

Environment controls include `--base-env=empty`, `--base-env=minimal`,
`--base-env=host`, `-e NAME[=VALUE]`, and `--workdir=PATH`.

## Logs And Run Summaries

Global logging options go before the subcommand:

```bash
hermit --log=info --log-file=hermit.log run --summary \
  --summary-json=summary.json -- PROGRAM
```

The equivalent environment variables are `HERMIT_LOG` and
`HERMIT_LOG_FILE`. Use `debug` or `trace` only for focused diagnosis; output can
be large.

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
run manifest, a small recording of those inputs, and names it in a diagnostic
(https://github.com/rrnewton/hermit/issues/4271):

```text
$ hermit run --seed-from=SystemRandom -- PROGRAM ARGS
hermit: this run drew epoch and seed from the host; its inputs are saved as recording 04cba9fc... in ~/.cache/hermit/04cba9fc...; rerun it with: hermit replay 04cba9fc...
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

Hermit needs unprivileged user, PID, and mount namespace support. Container
runtimes and hardened hosts may disable these facilities. A useful host probe
is:

```bash
unshare --user --map-root-user --pid --mount --fork true
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

### CPUID Or Hardware Warnings

CPUID faulting and PMU behavior differ across CPUs, VMs, and container hosts.
Record the CPU model and virtualization environment when reporting a failure.
`--no-virtualize-cpuid` may help diagnosis, but makes the guest depend on host
CPU features.

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
- Keep file contents and mount layout fixed.
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
