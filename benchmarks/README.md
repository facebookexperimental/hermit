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
--no-virtualize-cpuid --max-timeslice=disabled
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

The default is five measured samples plus one warmup for native, ptrace, DBT,
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

## Per-call system call cost

`getpid_cost.rs` measures what one raw `getpid` call costs natively and under
the `ptrace` and `liteinst` backends. It answers a narrower question than
`targeted.py`'s `syscall_heavy` row: how many microseconds each backend adds
to a single intercepted call once process start-up is taken out.

The guest is `fixtures/getpid_loop.c`, which makes exactly N raw `getpid`
calls and prints `calls=N`. Hermit virtualizes the guest's clocks, so the
harness times each whole run from outside. For each variant it takes the
median wall time at several N (default 0, 25,000, 50,000 and 100,000) and fits
a least-squares line through the medians at positive N. The slope is the cost
of one call; the intercept is the fixed cost of a run, including Hermit
start-up and any wrapper. The `extra us/call` column is a backend's slope
minus the native slope. A run with N = 0 never reaches the fixture's `getpid`
site, so it also skips that site's one-time cost: LiteInst traps and patches
the site at its first call, and every sample is a fresh process, so warmups
cannot remove that step. Fitting the zero-call median would add part of the
step to the slope (half of it at `--counts 0,2`), so the harness reports it
but leaves it out of the fit, and `--counts` needs at least two positive
values.

Build Hermit and the in-guest LiteInst runtime beside it first (the top-level
`README.md` describes the runtime), then run from the repository root:

```sh
cargo build --locked --release -p hermit --features liteinst --bin hermit
cargo build --locked --release -p detcore-liteinst
./benchmarks/getpid_cost.rs
./benchmarks/getpid_cost.rs --backends ptrace --counts 50000,100000 --iterations 3
./benchmarks/getpid_cost.rs --hermit "/path/to/wrapper target/release/hermit"
```

Hermit runs use the same hardware-independent configuration as `run.py`
(see Methodology above), so no PMU timer is armed. The slope is interception
plus Detcore's handling of `getpid`, not interception alone. The LiteInst
backend runs Detcore inside the guest, so a call through a patched site reaches
Detcore without a tracer stop. Each round runs
every count, and the variant order rotates from round to round so no variant
always runs first. A sample counts only if it exits 0 and prints exactly
`calls=N`; failed and timed-out samples are kept in the output, the variant
gets no fit, and the harness exits 1.

Every run starts with `RUST_LOG`, `HERMIT_LOG` and `HERMIT_LOG_FILE` removed
from its environment, so an inherited logging setting can neither slow the
timed runs nor move the statistics record off stderr. After timing, one more
run per backend at the largest N sets `RUST_LOG=hermit::backend_stats=debug`
and keeps the backend's own `backend run complete` record. LiteInst also gets
the same run with N = 0. Each of these runs must pass the same `calls=N` check
and print exactly one record naming its backend; otherwise that backend gets no
fit (its row reads `no fit` and its JSON summary has `"stats_accepted": false`
and `"fit": null`) and the harness exits 1. For LiteInst the record counts each
dispatch path: `direct_hook` for calls through a patched site, the trap paths
for calls that were not. `direct_hook` counts every patched site, so the
harness subtracts the zero-call run's count, which comes only from start-up
and exit sites. The harness prints the nonzero path counts beside the fits,
writes them to the JSON as `dispatch_paths`, and refuses the LiteInst result
the same way unless that difference is at least N - 1, the one allowance being
a first call that traps before its site is patched. Below that floor the
fixture's `getpid` site was not patched, so the LiteInst slope would be the
cost of a trap, not of a hooked call. A largest N below 2 would owe the hook
no call at all; two positive counts rule it out, and the check refuses it
anyway. Two limits apply to those counts:

- A call at a site that never had a patch attempt is in no path counter.
  LiteInst never attempts the task-creating calls (`clone`, `clone3`, `fork`,
  `vfork`), and once the guest has a second task it attempts no new site.
  After that point, calls through sites patched earlier still count as
  `direct_hook`, and calls at sites that had already fallen back still count
  as fallbacks; calls at sites first reached afterwards count nowhere. The
  fixture creates no task, so its counts are complete.
- Hermit prints the record only when the backend returns normally, including
  after a forced shutdown. A backend error or a timeout loses it, exactly as it
  does for `ptrace`; https://github.com/rrnewton/hermit/issues/3585
  tracks reporting the counters on those paths too.

Each wall time comes from polling the run every 500 microseconds, so a sample
can read up to one poll interval, plus the host's timer slack, longer than the
run. The JSON records the interval as `poll_interval_ns`.

Raw samples, per-count medians with median absolute deviation, the fits, the
statistics records, the commands, the full repository SHA, the script's
SHA-256, the host's kernel, CPU model and load average are written to the
ignored `benchmarks/results/getpid-cost.json`. The harness does not create a
cgroup or pin CPUs; run it on a quiet host and compare the recorded load
averages before trusting a small difference.
