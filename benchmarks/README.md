# Hermit performance benchmarks

This suite compares native wall-clock time with deterministic Hermit execution
for five representative workloads:

| Benchmark | Workload |
| --- | --- |
| `echo` | Process-launch baseline using `echo`. |
| `sort_1m_lines` | Sort one million deterministic lines. |
| `grep_large_file` | Search the same large input for a periodic marker. |
| `multithread_counter` | Four pthreads perform one million atomic increments each. |
| `fork_exec_chain` | A serial chain of 25 `fork` plus `exec` operations. |

## Run

From the repository root:

```sh
./benchmarks/run.py
```

The runner requires Python 3.9+, a C11 compiler, standard `echo`, `sort`, and
`grep` utilities, and the normal Hermit build prerequisites. It builds a
release Hermit binary and optimized C fixtures before timing. Generated inputs
and binaries live under `target/hermit-benchmarks/` and are not measured.

For a fast framework smoke test:

```sh
./benchmarks/run.py --iterations 1 --warmups 0 --sort-lines 10000
```

The standard run uses five measured iterations and one warmup for each native
and Hermit mode. It generates exactly one million lines. The runner alternates
which mode executes first on measured iterations to reduce ordering bias and
terminates any individual sample after 120 seconds.

## Methodology

Native and Hermit modes execute the same workload command with `LC_ALL=C`.
Hermit uses a hardware-independent deterministic configuration:

```text
--log=error run --base-env=minimal --env=LC_ALL=C
--no-virtualize-cpuid --preemption-timeout=disabled
```

Disabling PMU preemption makes the suite portable across supported Linux hosts
and keeps the measurement focused on deterministic syscall, process, and
thread handling. It does not measure chaos scheduling or PMU interrupt costs.
Guest stdout is sent to `/dev/null` in both modes so large sort output does not
pollute the terminal, while the full write path remains part of the workload.
A nonzero exit, missing prerequisite, or per-sample timeout fails the suite.

## Results

By default the runner writes ignored generated output to:

- `benchmarks/results/results.json`: schema-versioned configuration, commands,
  individual wall-clock samples, means, medians, and overhead percentages.
- `benchmarks/results/summary.md`: a human-readable table of mean native time,
  mean Hermit time, and overhead.

Overhead is calculated for each benchmark as:

```text
(Hermit mean / native mean - 1) * 100
```

A negative result is valid. In particular, deterministic thread serialization
can remove native atomic contention in the multithreaded counter workload.

Use `--output PATH` to retain multiple result sets outside the default ignored
directory. Use `--hermit PATH --skip-build` to benchmark a specific existing
Hermit executable.

## Targeted backend comparison

`targeted.py` isolates four backend cost shapes:

| Benchmark | Fixed workload | Intended signal |
| --- | --- | --- |
| `cpu_bound` | 1,000,000 arithmetic iterations and no loop syscalls | Instruction execution and deterministic preemption cost. |
| `syscall_heavy` | 100,000 raw calls, alternating `getpid` and `clock_gettime` | Per-syscall interception cost. |
| `large_startup` | Traverse a 4 MiB executable text path once | Large-image translation and process startup cost. |
| `mixed_workload` | 10,000 compute blocks, each followed by raw `getpid` | Amortized compute plus interception cost. |

Run the complete matrix from the repository root:

```sh
with-proxy ./benchmarks/targeted.py
```

The default is five measured samples plus one warmup for native, ptrace, DBI,
and KVM. Every Hermit command uses explicit `--strict`, `--log=error`, and
no determinism relaxations. Before timing, the runner requires each backend to
exit zero and produce byte-identical stdout to native. A backend or workload
that fails this precheck is recorded as unavailable rather than misreported as
a fast sample.

The runner reports medians and ratios against the native median. Raw samples,
commands, host metadata, and failure reasons are written under the ignored
`benchmarks/results/targeted/` directory. Use `--backends`, `--benchmarks`,
`--iterations`, and `--output` to select a smaller matrix or preserve
multiple result sets. For example:

```sh
./benchmarks/targeted.py --skip-build --iterations 1 --warmups 0 \
  --backends native,ptrace --benchmarks cpu_bound
```
