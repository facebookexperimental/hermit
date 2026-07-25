# KVM and ptrace performance parity

This experiment compares Hermit's KVM and ptrace backends with the same
deterministic CLI configuration. It measures end-to-end application latency,
incremental syscall interception cost, host context switches, and streaming
throughput.

## Methodology

Every guest uses these common arguments:

```text
hermit --log=off run --backend BACKEND --strict \
  --base-env=minimal --tmp=/tmp -- PROGRAM ARGUMENTS...
```

The timing process and its descendants are pinned to one logical CPU. If
`--cpu` is omitted, the runner samples `/proc/stat` for 250 ms and chooses the
least-busy CPU in its allowed affinity set. Backend order alternates on every
sample so drift does not consistently favor one backend.

Before timing, both backends must exit successfully and produce byte-identical
stdout for every workload. Expected stdout is also checked where it is stable.
Timed stdout and stderr go to `/dev/null`; logging is disabled equally for both
backends. The default run performs two warmups and nine measured samples, then
reports median, p95, mean, and standard deviation.

The metrics are:

- **Application latency:** complete Hermit invocation, including backend setup,
  guest loading, execution, and teardown.
- **Syscall interception:** 10,000 raw `getpid` calls minus a zero-call run of
  the same fixture, divided by 10,000.
- **Pipe boundary:** 1,000 one-byte pipe write/read round trips minus a
  zero-round-trip run, divided by 2,000 supervisor boundaries.
- **Host context switches:** `ru_nvcsw + ru_nivcsw` from the cumulative child
  process tree, with the corresponding zero-operation baseline subtracted.
- **Throughput:** 16 MiB emitted by `/bin/cat` divided by gross end-to-end wall
  time. It intentionally includes backend startup.

The pipe metric is a supervisor-boundary cost, not an application thread
switch. KVM cannot yet execute the pthread ping-pong workload, so presenting a
thread-switch comparison would not be parity testing.

## Workloads

The application set contains `/bin/echo`, `/bin/ls`, `/bin/cat`, and passing
programs from `validate.sh`: `true`, `pwd`, `seq`, `head`, `base64`, `id`, and
`printf`. The runner builds two small C fixtures for syscall and pipe-boundary
measurement. Directory and stream inputs are generated deterministically.

## Running

Build an optimized Hermit binary:

```bash
with-proxy cargo build --release -p hermit
```

Run the default experiment:

```bash
python3 experiments/kvm-perf-parity/run_benchmarks.py \
  --output-dir /tmp/kvm-perf-parity
```

For a longer run on an explicitly selected CPU:

```bash
python3 experiments/kvm-perf-parity/run_benchmarks.py \
  --samples 15 --warmups 3 --cpu 8 \
  --output-dir /tmp/kvm-perf-parity
```

The output directory contains:

- `metadata.json`: Hermit hash, source revision, host, CPU, load, and settings.
- `raw.tsv`: every timing and resource-usage observation.
- `summary.tsv`: descriptive statistics by workload and backend.
- `derived.tsv`: startup-subtracted boundary metrics and gross throughput.

Useful controls include `--syscall-iterations`, `--pipe-iterations`,
`--stream-bytes`, `--timeout`, and `--hermit`.

## Interpretation limits

Both commands request strict determinism, and this corpus is restricted to
workloads that pass output-parity preflight. That does not mean the backends
currently provide identical guarantees for all Linux programs. The KVM
personality has one vCPU and does not yet implement general thread/process
lifecycle, PMU preemption, signals, or page-permission faults. Ptrace remains
the baseline for those behaviors.

`RUSAGE_CHILDREN` counts Linux scheduler switches in the Hermit process tree.
It captures ptrace tracer/tracee wakeups. A KVM exit returns to the same host
thread and therefore normally does not create a Linux context switch. The
metric demonstrates that architectural distinction; it is not a hardware
VM-exit counter.

Performance measurements on shared hosts remain sensitive to scheduler load.
Always retain `metadata.json`, inspect p95 and standard deviation, and compare
raw samples before treating small application-level differences as meaningful.
The microbenchmarks use enough operations to dominate startup and provide the
strongest backend signal.

The checked-in observation and raw samples are documented in
[`RESULTS.md`](RESULTS.md) and [`results/2026-07-24`](results/2026-07-24/).
