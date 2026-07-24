# Backend performance benchmarks

This suite compares Hermit's ptrace, DynamoRIO (DBI), and KVM backends on the
same micro and substantial application workloads. Native execution is included
as a reference. The runner records capability failures separately and only
times backend/workload pairs that pass semantic preflight.

The checked-in observation is summarized in [`RESULTS.md`](RESULTS.md), with
machine-readable evidence under [`results/2026-07-24`](results/2026-07-24/).

## Prerequisites

Build the complete release package, including the DBI runtime cdylib:

```bash
with-proxy cargo build --release -p hermit
```

Do not add `--bin hermit` to this command. A bin-only build can refresh the
executable without refreshing top-level `target/release/libhermit.so`; the DBI
native client deliberately links that library and requires an ABI-matched
artifact.

The host needs x86-64 Linux, GNU `/usr/bin/time`, a C/C++ toolchain, CMake,
Python 3, bzip2, SQLite, and read-write `/dev/kvm` access for KVM rows. Prepare
pinned Ninja and LevelDB binaries with:

```bash
experiments/backend-benchmarks/prepare_dependencies.sh
```

The preparer checks out Ninja v1.13.1 (`79feac0f`) and LevelDB 1.23
(`99b3c03b`) below ignored `target/backend-benchmarks/deps/`. It uses
`with-proxy` when that command is available and refuses to reuse a checkout at
an unexpected commit.

## Running

Run the default matrix from the repository root:

```bash
experiments/backend-benchmarks/run_benchmarks.py \
  --output-dir /tmp/hermit-backend-benchmarks
```

The default experiment uses one warmup and five measured samples. The checked-in
observation used seven samples and a 30-second capability timeout:

```bash
experiments/backend-benchmarks/run_benchmarks.py \
  --samples 7 --warmups 1 --timeout 30 \
  --output-dir /tmp/backend-bench-final
```

Useful controls include `--cpu`, repeatable `--mode`,
`--syscall-iterations`, `--fork-iterations`, `--bzip2-bytes`,
`--ninja-jobs`, `--leveldb-operations`, and `--sqlite-rows`.

## Workloads

| Workload | Category | Default work |
| --- | --- | ---: |
| `true` | startup | one empty process |
| `syscall-loop` | syscall micro | 2,000 raw `getpid` calls |
| `fork-exec` | process micro | 16 `fork` + `execve(/bin/true)` + `waitpid` cycles |
| `bzip2-2m` | compute macro | bzip2 level 9 over deterministic 2 MiB input |
| `ninja-graph` | process macro | Ninja `-j1` graph with 32 generated outputs |
| `leveldb-fillread` | storage macro | 2,000 sequential writes and 2,000 random reads |
| `sqlite-insert-index` | storage macro | 20,000 inserts, index build, and aggregate query |

Zero-operation versions of the syscall and fork fixtures provide
startup-matched baselines for incremental costs. Stateful workloads are reset
outside the timed interval before every run.

## Methodology

Every Hermit mode uses:

```text
hermit --log=off run --backend MODE --strict \
  --base-env=minimal --tmp=/tmp -- PROGRAM ARGUMENTS...
```

The runner applies a small fixed host environment (`LC_ALL=C`, `TZ=UTC`, and
basic identity/path variables) to native and Hermit commands. It then:

1. Chooses the least-busy allowed logical CPU over a 250 ms sample unless
   `--cpu` is specified.
2. Pins GNU time, Hermit, the guest, and inherited backend threads to that CPU.
3. Preflights native and every requested backend, checking exit status, stdout
   parity or a semantic marker, and stateful output artifacts.
4. Records `failed`, `timeout`, or `blocked` pairs in `compatibility.tsv` and
   excludes them from timing.
5. Rotates mode order on every measured sample so host drift does not always
   favor one backend.
6. Uses a new process group per command. On timeout it sends TERM, waits one
   second, and unconditionally kills the entire group so DynamoRIO descendants
   cannot survive the benchmark.

Wall time comes from `time.perf_counter_ns`. GNU time records user CPU, system
CPU, peak RSS, and voluntary/involuntary context switches for the command
process tree. Summary statistics are median, p95, mean, and sample standard
deviation. Ratios use medians; raw samples remain authoritative on noisy hosts.

## Evidence files

- `compatibility.tsv`: semantic preflight result and diagnostic for every pair.
- `raw.tsv`: every measured wall/CPU/RSS/context-switch observation.
- `summary.tsv`: descriptive statistics for every passing pair.
- `derived.tsv`: startup-subtracted micro costs and cross-backend speedups.
- `metadata.json`: source/binary hashes, host, CPU, load, parameters, dependency
  commits, and exact workload commands.

## Interpretation limits

A failed or timed-out pair is a backend capability result, not a zero-time
measurement. KVM currently has a single-process execution personality and
cannot run general fork/thread workloads. Its filesystem syscall coverage is
also narrower than ptrace. DBI runs Detcore in-process but still has lifecycle
and threaded-I/O gaps. Ptrace remains the compatibility baseline.

The native reference does not claim deterministic behavior. It exists only to
quantify backend overhead for identical application work. Hermit does not make
a changing external filesystem deterministic, so all benchmark state is
created under the repository's ignored `target/` tree and reset for each run.
