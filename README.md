# Hermit

Hermit is a deterministic execution environment for x86-64 Linux programs. It
runs an unmodified guest under the
[Reverie](https://github.com/facebookexperimental/reverie) ptrace backend and
controls sources of nondeterminism including thread scheduling, time, random
data, CPUID results, and selected file metadata.

Hermit is useful for repeatable execution, controlled concurrency testing,
record/replay experiments, and diagnosing schedule-sensitive failures.

## Requirements

Hermit currently supports x86-64 Linux. Building and running it requires:

- Rust nightly through [rustup](https://rustup.rs/); `rust-toolchain.toml`
  selects the repository toolchain automatically.
- Linux user, PID, and mount namespaces.
- Parent-child ptrace and seccomp filter support.
- libunwind and LZMA development packages.
- User-space performance counters for precise scheduler preemption. Hermit can
  run without them, but CPU-bound workloads receive fewer preemption points.

On Debian or Ubuntu:

```bash
sudo apt-get update
sudo apt-get install -y libunwind-dev liblzma-dev
```

On Fedora or CentOS:

```bash
sudo dnf install -y libunwind-devel xz-devel
```

## Install From Source

Clone the maintained fork and install the CLI into Cargo's binary directory,
normally `~/.cargo/bin`:

```bash
git clone https://github.com/rrnewton/hermit.git
cd hermit
cargo install --path hermit-cli
hermit --version
```

To build without installing:

```bash
cargo build --workspace
./target/debug/hermit --version
```

An optimized workspace build also assembles every backend runtime into one
installation staging directory:

```bash
cargo build --release
./target/install_pkg/hermit --version
```

`target/install_pkg/rsrcs/` contains the SaBRe and e9patch rewriters, Detcore
backend shared libraries, the DynamoRIO launcher/runtime, and its relocatable
client. Hermit finds that directory from its invocation path or executable
path. Set `HERMIT_INSTALL_DIR` only when the resources live under a different
prefix. The staging `hermit` entry is a symlink to `target/release/hermit`; use
a dereferencing copy such as `cp -aL target/install_pkg/ DESTINATION` when
making a standalone installation or archive.

## Quick Start

Run a command deterministically by placing `hermit run --` before it:

```bash
hermit run -- /bin/echo hello
```

The `--` separator is recommended so arguments beginning with `-` are passed to
the guest. The command above prints `hello` and exits with the guest's status.

Hermit's current defaults are strict and deterministic. `--strict` is retained
as an explicit compatibility spelling for those defaults; it does not enable a
stronger mode:

```bash
hermit run --strict -- /bin/echo hello
```

### Execution Backends

Hermit accepts `--backend=ptrace|dbi|liteinst|sabre|kvm|e9patch` as a global
option before the subcommand. Backend scope is command-specific: LiteInst and
e9patch support only `run`, while SaBRe supports `run` and `strace`; unsupported
combinations fail closed. Omitting the option selects `ptrace`, preserving the
existing behavior:

```bash
hermit --backend=ptrace run -- /bin/echo hello
```

For backwards compatibility, `run` still accepts `--backend` after the
subcommand (`hermit run --backend=ptrace -- /bin/echo hello`).

Backend selection fails closed. Hermit never substitutes ptrace after an
explicit backend request. LiteInst is an experimental ptrace-hosted hybrid for
dynamically linked Linux x86-64 guests:

```bash
./scripts/stage-liteinst-runtime.sh dev \
  "$PWD/target/debug/libreverie_liteinst.so" \
  "$PWD/target/liteinst-runtime-build"
cargo build --locked -p hermit --bin hermit
./target/debug/hermit run --backend=liteinst --strict --verify -- /bin/echo hello
```

The ptrace host owns the sole generic Reverie `Detcore` Tool and GlobalTool.
The standalone manifest enables and statically verifies the preload constructor;
Hermit rejects non-runtime or constructor-free overrides before activation.
The resulting Reverie preload DSO initializes only the LiteInst patch/helper
side; it never installs another Tool in the guest. The host observes the first
invocation of each eligible syscall site and installs an instruction-punning
hook. Later invocations enter the LiteInst trampoline and return to the same
ptrace-owned Detcore lifecycle.

`--verify` runs the normal Detcore comparison over captured status, output,
and deterministic scheduler logs, so a successful result is an L2 claim.
Current support is limited to single-threaded, single-process guests. Thread
clone, `fork`, and `vfork` fail closed with `EOPNOTSUPP`; `exec` is also
unsupported because runtime rebootstrap after image replacement is not yet
implemented. RCB preemption and CPUID/RDTSC interception use the ptrace host
and retain its PMU and CPU capability requirements.
The default Hermit namespace path is supported; `--no-namespace` remains an
explicit option for trusted guests. The in-guest patch runtime is experimental
and continues to receive compatibility and lifecycle improvements.
The release installation package supplies the DynamoRIO, SaBRe, LiteInst, and
e9patch runtime artifacts. KVM requires read-write `/dev/kvm` access plus its
guest-kernel Linux ABI.

SaBRe is built only with the non-default `third-party-backends` feature. Its
measured post-0.2 strict-verify envelope, build instructions, and explicit
unsupported cases are documented in
[SaBRe backend compatibility](docs/SABRE_COMPATIBILITY.md).

The experimental `e9patch` selection is intentionally a hybrid backend. At
startup it loads or generates the main ELF's cached instruction map, then runs
`e9tool -O0` with exact file-offset matches to install semantics-preserving
trampolines at every candidate offset that e9tool recovers as an instruction.
The conservative optimizer setting avoids known multi-class rewrite failures.
The linear scan can include embedded data, so Hermit reports candidate and
recovered counts separately. Partial e9tool coverage fails closed. Hermit does
not enable e9patch's B0 fallback because it reserves SIGILL and changes guest
signal semantics. The rewritten ELF still runs through Detcore's ptrace backend,
which executes the original instructions and covers trapped events in shared
libraries, the vDSO, and dynamic code. Raw `RDRAND`, `RDSEED`, and TSX in code
remain unsupported even when present in the offline map because this initial
integration installs empty trampolines. Privilege-bearing executables fail
closed rather than losing set-ID or file-capability semantics. This
establishes the cached-rewrite pipeline but does not yet reduce ptrace events.
Install `e9tool` in `PATH` or set `HERMIT_E9TOOL` to its executable.
Non-ELF entrypoints, including shebang scripts, skip preprocessing and run
through the ptrace correctness path.

A quick determinism check is to run the same virtual random-data read twice:

```bash
hermit run -- /bin/sh -c 'od -An -N8 -tx1 /dev/urandom'
hermit run -- /bin/sh -c 'od -An -N8 -tx1 /dev/urandom'
```

Both invocations should print the same bytes when the command, inputs, and
Hermit configuration are unchanged.

## Compatibility

Hermit can run substantial multi-process applications, but unsupported
syscalls and host-specific CPU behavior remain. One compatibility milestone is
booting a minimal x86_64 Linux system under QEMU TCG. The working profile uses
QEMU's instruction-counting clock and lets QEMU's host threads run
concurrently:

- [Booting Linux with QEMU under Hermit](docs/QEMU_BOOT.md)

That profile is a boot compatibility demonstration, not a fully deterministic
virtual-machine configuration.

## Key Workflows

| Goal | Command | Status |
| --- | --- | --- |
| Deterministic execution | `hermit run -- PROGRAM ARGS...` | Default and recommended mode |
| Verify two executions | `hermit run --verify -- PROGRAM` | Compares output, status, and deterministic logs |
| Explore schedules | `hermit run --chaos --sched-seed=N -- PROGRAM` | Seeded, reproducible schedule variation |
| Record an execution | `hermit record start -- PROGRAM ARGS...` | Experimental |
| Replay the latest recording | `hermit replay --autopilot` | Experimental |
| Diagnose a concurrency failure | `hermit analyze --search -- PROGRAM` | Advanced, may run the guest many times |

A minimal record/replay session is:

```bash
hermit record start -- /bin/echo recorded
hermit replay --autopilot
```

### Chaos Mode Demonstration

The `order_violation` guest reads shared state without ensuring that another
thread has published it. Build Hermit and the guest, then run the default
deterministic schedule twice:

```bash
cargo build --release -p hermit --bin hermit
mkdir -p target/chaos-demo
cc -std=c11 -O2 -pthread tests/chaos/order_violation.c \
  -o target/chaos-demo/order-violation

for run in 1 2; do
  target/release/hermit run --max-timeslice=disabled -- \
    ./target/chaos-demo/order-violation
done
```

Both runs print `Hello world!`. Chaos mode explores other deterministic
schedules; this bounded search reports the guest status for each seed:

```bash
for seed in {0..15}; do
  target/release/hermit run --chaos --sched-heuristic=random \
    --max-timeslice=disabled --seed="$seed" -- \
    ./target/chaos-demo/order-violation
  printf 'seed=%s status=%s\n' "$seed" "$?"
done
```

With this Hermit revision and guest, seed 9 prints
`ERROR! global_str is null at use.` and exits 1. Repeating the exact command
reproduces the same failure:

```bash
target/release/hermit run --chaos --sched-heuristic=random \
  --max-timeslice=disabled --seed=9 -- \
  ./target/chaos-demo/order-violation
```

The seed identifies a schedule for a particular Hermit build, guest binary,
and configuration. Keep all three fixed when reproducing a failure. This demo
disables PMU preemption and relies on thread and syscall scheduling points, so
it also works on hosts without accessible performance counters.

Record/replay is less broadly compatible than deterministic `run` mode. Keep
the recording directory, executable, inputs, environment, and Hermit revision
unchanged between phases.

## Compatibility

The following matrix summarizes unmodified host-binary testing on x86-64 Linux
as of 2026-07-21. "Verified" describes the named probe, not every workflow a
program supports. Run and record/replay results are intentionally separate.

Some launch probes disabled CPUID virtualization and PMU preemption to match
the test host's capabilities; the linked report records the exact flags.

| Program or workload | Deterministic run | Record/replay | Scope |
| --- | --- | --- | --- |
| `/bin/echo` | Verified | Verified | Output and exit status match |
| `ls`, `cat`, `grep`, `sed`, `awk`, `sort`, `wc` | Verified | Verified for tested file fixtures | Inputs must remain stable and visible in the guest |
| `sh -c` shell built-ins | Verified | Verified | Child-process pipelines have additional limitations |
| System Python 3 | Verified for `print` and tested file/JSON work | Verified for simple `print`; limited for complex imports and subprocesses | Some recording paths remain incomplete |
| Node.js 16 | Verified for `console.log` | Limited; tested record/replay hangs | Basic launch works; this is not full Node compatibility |
| OpenJDK 8 | Verified for `java -version` | Limited; replay hangs | Version probe only |
| curl, wget, Git, GCC | Verified for version probes | Verified for version probes; functional workflows vary | External network and child-process behavior need separate testing |
| SQLite | Verified for an in-memory query | Limited; replay diverges | Filesystem-event replay remains incomplete |

See the full [arbitrary binary compatibility matrix](ai_docs/arbitrary-binary-matrix.md)
for exact commands, host details, functional workloads, and linked issues.
Compatibility evolves with syscall coverage, so validate the smallest real
workload you depend on rather than relying on a version probe alone.

## Performance

Hermit's deterministic ptrace backend should generally be budgeted at roughly
3-6x native wall-clock time. This is a planning range, not a benchmark promise:
overhead varies with syscall frequency, thread count, PMU availability, and the
amount of scheduling and logging enabled.

`--strict` uses the normal deterministic defaults and has the same performance
profile as a default run. Chaos, verify, record/replay, and analyze modes may
perform multiple executions or retain additional events, so their total cost
can be higher. Benchmark your actual workload on the deployment CPU and kernel.

## Architecture

Hermit has three main layers:

1. The `hermit` CLI validates configuration and creates the guest namespaces,
   mounts, environment, and process tree.
2. Reverie uses ptrace and seccomp-assisted interception to stop and resume the
   guest around subscribed syscalls and CPU events.
3. Detcore applies deterministic policy: it virtualizes selected results,
   serializes threads, models resources and logical time, and records or
   replays external inputs.

Linux still performs most operations. Hermit is a determinization layer, not a
replacement kernel or sandbox. See the [architecture guide](docs/ARCHITECTURE.md)
for the event lifecycle, state ownership, scheduler, resource model, virtual
time, and record/replay design.

## Troubleshooting

Hosts and container runtimes commonly block namespaces, ptrace, seccomp, or
`perf_event_open`. Start with:

```bash
hermit run --namespace-only -- /bin/true
hermit --log=info run --strace-only -- /bin/true
```

These are diagnostic modes and do not provide normal determinism. The
[User Guide](docs/USER_GUIDE.md#troubleshooting) covers host setup, PMU access,
program visibility, hangs, verification differences, and record/replay. The
[Error Catalog](docs/ERROR_CATALOG.md) maps stable error text to causes and
fixes.

## Contributing

Focused contributions are welcome. Before opening a pull request:

1. Fork the repository and create a branch from `main`.
2. Add a focused regression test for behavior changes.
3. Keep generated manifests and documentation consistent with the source.
4. Run formatting and the broadest tests your Linux host supports:

   ```bash
   cargo fmt --all -- --check
   cargo test -p AFFECTED_PACKAGE
   cargo test --workspace
   ```

5. Document host-dependent skips or failures instead of weakening the test.

Not every x86-64 Linux machine can run every Hermit test: PMU counters, CPUID
faulting, `perf_event_open` permissions, and user/mount namespaces each gate
different test groups, so results differ across bare metal, VMs, containers, and
WSL. Before reporting an environment-related failure, see
[Testing Environments](docs/TESTING_ENVIRONMENTS.md) for the support matrix, the
list of hardware-sensitive tests, expected failure signatures, and a copy-paste
diagnostic and bug-report procedure.

See [CONTRIBUTING.md](CONTRIBUTING.md) for the pull-request, CLA, issue, style,
and licensing guidelines.

## More Documentation

- [User Guide](docs/USER_GUIDE.md): modes, flags, examples, and troubleshooting.
- [Architecture](docs/ARCHITECTURE.md): Reverie, Detcore, scheduling, time, and
  record/replay internals.
- [e9patch Compatibility](docs/E9PATCH_COMPATIBILITY.md): measured application
  envelope, preprocessing classifications, and known limits.
- [Error Catalog](docs/ERROR_CATALOG.md): errors, triggers, and remediations.
- [Per-test Hermit Code Coverage](docs/HERMIT_CODE_COVERAGE.md): measure and
  diff the Hermit/Detcore implementation paths exercised by a test.
- [Examples](examples/README.md): small programs demonstrating controlled
  nondeterminism.
- [License](LICENSE): BSD 3-Clause.
