# Hermit User Guide

Hermit runs Linux programs in a controlled environment and replaces or
sanitizes sources of nondeterminism such as time, random data, file metadata,
and thread scheduling. Its main uses are reproducible execution, controlled
concurrency testing, and experimental record/replay debugging.

Hermit is not a security boundary. The guest can see most of the host file
system by default, and changes to file contents can change a run. Complete
reproducibility still requires fixed input files and no external network
dependency.

Hermit is in maintenance mode. Compatibility is substantial but incomplete,
especially for less common Linux system calls.

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
sudo apt-get install -y libunwind-dev liblzma-dev
```

On Fedora or CentOS:

```bash
sudo dnf install -y libunwind-devel xz-devel
```

Hermit may work in a VM or container if the host exposes the required kernel
facilities. Nested containers commonly block namespaces, `ptrace`, seccomp, or
`perf_event_open`; see [Troubleshooting](#troubleshooting).

## Getting Started

### Build And Install

From the repository root, build the workspace:

```bash
cargo build --workspace
./target/debug/hermit --version
```

The debug executable is `target/debug/hermit`. For an optimized build:

```bash
cargo build --release -p hermit --bin hermit
./target/release/hermit --version
```

To install the current checkout into Cargo's binary directory, normally
`~/.cargo/bin`:

```bash
cargo install --path hermit-cli
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
- gives the guest an isolated PID namespace and `/tmp`;
- uses an isolated local network namespace;
- exposes most of the host file system read/write.

For a cleaner starting environment, use a minimal environment and pass only
the variables the program needs:

```bash
hermit run --base-env=minimal -e LANG=C --workdir=/tmp -- /bin/pwd
```

#### Backend Selection

Use `--backend=ptrace|dbi|kvm` to select the process instrumentation backend.
It is a global option and belongs before the subcommand, because the backend
governs how any subcommand instruments the guest. The default is `ptrace`, so
existing commands are unchanged:

```bash
hermit --backend=ptrace run -- /bin/echo hello
```

For backwards compatibility, `run` also accepts `--backend` after the
subcommand (`hermit run --backend=ptrace -- /bin/echo hello`).

Hermit detects whether the requested backend is integrated and available on
the current host. It does not silently fall back to a different backend. The
current DynamoRIO prototype requires a discoverable SDK and has no Detcore
process launcher. The bare KVM prototype requires read-write `/dev/kvm` access,
commonly through the `kvm` group or root, plus a guest-kernel ABI. Requests for
those prototypes therefore fail before the guest starts and explain the missing
capability.

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
available. `--preemption-timeout=N` controls the longest uninterrupted virtual
time slice. Smaller values create more scheduling opportunities at additional
runtime cost. `--preemption-timeout=disabled` avoids PMU use but can miss bugs
in CPU-bound code that rarely makes system calls.

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
the executions differ or if the guest exit status is not allowed.

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

List recordings:

```bash
hermit record list
hermit record list --json
```

Replay a recording to completion without a debugger:

```bash
hermit replay --autopilot RECORDING_ID
```

Omit the ID to replay the last successful recording:

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
| `--passthru-opt` | Use the reduced syscall subscription set for performance. Unlisted syscalls bypass Detcore, weakening deterministic accounting. |
| `--no-sequentialize-threads` | Lets Linux schedule guest threads concurrently. This weakens schedule reproducibility. |
| `--no-deterministic-io` | Disables Hermit's deterministic short-I/O completion behavior. |
| `--chaos` | Uses seeded randomized deterministic scheduling. |
| `--sched-seed=N` | Selects a reproducible chaos schedule. |
| `--preemption-timeout=N` | Sets the maximum virtual time slice and requires PMU support. |
| `--preemption-timeout=disabled` | Disables PMU timer preemption. |

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
time slice; with `--preemption-timeout=disabled`, a thread that never reaches an
intercepted event can starve its peers. No separate fairness flag is needed for
the default equal-priority policy.

### Virtual Inputs

| Option | Effect |
| --- | --- |
| `--seed=N` | Sets the fallback random seed. |
| `--rng-seed=N` | Sets only guest random-data generation. |
| `--epoch=TIMESTAMP` | Sets the virtual clock's RFC 3339 starting time. |
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

`--namespace-only` runs the command in Hermit's namespaces without ptrace,
seccomp interception, or determinization. It helps separate namespace setup
failures from interception failures.

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

Hermit prints a warning and continues with `--preemption-timeout=disabled`
when `perf_event_open` is unavailable. Check:

```bash
cat /proc/sys/kernel/perf_event_paranoid
```

The host setting, VM PMU exposure, and container seccomp policy can all block
performance counters. Enable them when precise scheduling preemption matters.
Otherwise pass `--preemption-timeout=disabled` explicitly and understand that
CPU-bound threads may run until another intercepted event.

### Unsupported System Calls

Hermit implements deterministic behavior for many, but not all, Linux system
calls. By default, an unimplemented call is passed through to Linux. That can
restore compatibility while introducing nondeterminism.

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

Some calls are fully emulated, some are forwarded and sanitized, and unsupported
calls may pass through. This is why Hermit can run ordinary unmodified binaries
but cannot guarantee determinism for every Linux interface.

## Reproducibility Checklist

Before treating a result as reproducible:

- Use the same Hermit revision, executable, arguments, and configuration.
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
