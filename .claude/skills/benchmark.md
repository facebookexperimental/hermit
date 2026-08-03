---
name: benchmark
description: "Run and publish focused, reproducible Hermit benchmarks with cgroup isolation, a K-core no-pinning protocol, median-based results, slowdown decomposition, full-SHA metadata, and mini-paper reporting. Use for benchmark design, execution, review, or publication."
---

# Benchmark

Use this protocol for performance work. Also load
[presenting-quantitative-data](presenting-quantitative-data.md) and apply its
ratio, precision, source-linking, and reader-audit rules.

## Experimental Shape

- Start with a targeted-small experiment: the fewest workloads and variants
  that answer one stated question. Expand only after the mechanism and
  correctness gates are understood.
- Run every compared variant on the same host and input. Rotate or randomize
  variant order when practical, and record the order and ambient load.
- Place the complete benchmark process tree in a dedicated cgroup. Record its
  CPU, memory, and process limits plus competing cgroups. Isolation is not a
  claim of exclusive hardware unless exclusivity was actually enforced.
- Use a K-core cgroup allocation with `K=1` by default. Do not pin individual
  tasks or threads: let the kernel schedule and migrate them within the K-core
  set. Record K and the exact CPU set. Increase K only when the research
  question requires parallel capacity.
- Gate every timed sample on exit status and workload-specific correctness.
  Keep failures and timeouts in the raw results.

## Sampling And Statistics

- Record warmups and measured repetitions explicitly. Retain every raw sample.
- Report the median as the primary timing statistic. Include sample count and
  a compact dispersion measure such as IQR or median absolute deviation.
- Use a same-collection native baseline. Do not rank measurements from
  different hosts or materially different workload configurations.
- Report absolute time before or beside normalized slowdown, using defensible
  significant figures.

## Separate Slowdown Sources

Measure compatible configurations that isolate these stages when the system
supports them:

| Symbol | Configuration |
| --- | --- |
| `T_native` | Native workload baseline. |
| `T_instr` | Instrumentation/backend active, thread sequentialization disabled. |
| `T_full` | Instrumentation/backend active with sequentialization enabled. |

Report **instrumentation-only slowdown first**:

1. instrumentation-only: `median(T_instr) / median(T_native)`;
2. incremental sequentialization: `median(T_full) / median(T_instr)`;
3. total configured slowdown: `median(T_full) / median(T_native)`.

Keep the commands and all non-target settings identical. If a configuration
cannot isolate one factor, mark the result confounded; do not estimate a
component by assertion. State semantic differences between native,
instrumented, and deterministic execution.

## Mini-Paper Artifact

Publish a concise README with these sections:

1. **Provenance**: UTC run time, run ID, short hostname, full product and
   harness SHAs via metadata, dirty state, tool versions, and raw-data links.
2. **Methods**: exact commands, cgroup setup, K-core set, no-pinning policy,
   workload inputs, order, warmups, repetitions, timeout, correctness gates,
   and statistic definitions.
3. **Evaluation**: what each workload exercises and why each variant answers
   the stated question.
4. **Results**: absolute medians, dispersion, sample counts, instrumentation
   slowdown first, sequentialization increment, total slowdown, failures,
   limitations, and clearly labeled hypotheses.

Keep full rows in CSV/TSV/JSON and link them from the README.

## Metadata And Privacy

Store machine-readable `metadata.json` with full 40-hex repository SHAs,
producing-script SHA-256, input digests, run ID, UTC timestamps, dirty state,
short hostname, kernel/hardware facts, cgroup configuration, K and CPU set,
commands, order, warmups, repetitions, timeouts, and tool versions. A short SHA
may appear in prose only when the metadata supplies the full immutable value.

Persist only the first DNS label. Scrub internal FQDNs from Markdown, metadata,
tables, logs selected for check-in, and generated reports. Scan the complete
artifact before publication and treat any FQDN hit as a release-blocking error.

## Publication Check

- Parse structured artifacts and recompute every summary from raw rows.
- Confirm compared rows share host, inputs, commands, and measurement anchor.
- Verify cgroup membership, K-core/no-pinning settings, correctness gates,
  sample counts, medians, dispersion, and slowdown formulas.
- Verify full SHAs, producer hash, short hostname, and FQDN scrub.
- Inspect the exact staged diff; never commit binaries or high-volume raw logs.
