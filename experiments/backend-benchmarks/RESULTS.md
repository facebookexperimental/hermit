# Backend benchmark results: 2026-07-24

This observation compares native, ptrace, DBI, and KVM execution on Hermit
`5e66b600097deefb09c78510228815a0cbd2f83c`. The release Hermit binary SHA-256
was `e981f212b38a8b2af62683e326866a6545a2aaaf6f520a220c57a073a8962a85`.
The benchmark harness was uncommitted during measurement, so `metadata.json`
records both the base commit and the exact runner SHA-256.

## Environment

| Setting | Value |
| --- | --- |
| Host CPU | AMD EPYC 9D85, 316 logical CPUs |
| Pinned CPU | 41, selected as least busy over 250 ms |
| Governor | performance |
| Kernel | `6.17.13-0_fbk0_crackerjackhost_0_g2b4321c50d79` |
| Load average at metadata capture | 343.60 / 402.04 / 418.66 |
| Samples | 1 warmup + 7 measured, rotating mode order |
| Capability timeout | 30 seconds |
| Ninja | v1.13.1, `79feac0f3e3bc9da9effc586cd5fea41e7550051` |
| LevelDB | 1.23, `99b3c03b3284f5886f9ef9a4ef703d57373e61be` |

The machine was heavily loaded. Medians carry the main comparisons, while p95,
standard deviation, and all 210 observations are retained in `summary.tsv` and
`raw.tsv`. Small differences should not be generalized from this host.

## Wall time

Median end-to-end milliseconds. `TIMEOUT` means the semantic preflight exceeded
30 seconds, so no timed samples were collected. `FAIL` means preflight returned
a concrete capability error.

| Workload | native | ptrace | DBI | KVM |
| --- | ---: | ---: | ---: | ---: |
| `/bin/true` | 5.718 | 66.509 | 218.511 | 67.694 |
| 2,000 raw `getpid` calls | 5.809 | 1,169.538 | 216.951 | 116.572 |
| 16 fork/exec/wait cycles | 18.396 | 766.966 | 2,527.499 | FAIL |
| bzip2, deterministic 2 MiB | 366.909 | 23,637.400 | 3,877.266 | 467.319 |
| Ninja, 32-output graph | 116.614 | 8,240.184 | TIMEOUT | FAIL |
| LevelDB, 2K fill + 2K read | 66.816 | 2,124.661 | TIMEOUT | FAIL |
| SQLite, 20K rows + index | 66.298 | 2,027.315 | 967.914 | FAIL |

## Comparisons

- **bzip2:** DBI was **6.10x** faster than ptrace; KVM was **50.58x** faster.
  KVM was 1.27x native, while ptrace was 64.42x native.
- **SQLite:** DBI was **2.09x** faster than ptrace. KVM failed before timing.
- **gross syscall loop:** DBI was **5.39x** faster and KVM **10.03x** faster
  than ptrace for the same 2,000-call command.
- **incremental syscall cost:** ptrace measured **551.84 us/call** and KVM
  **25.11 us/call**, a **21.97x KVM advantage**. DBI's active median was below
  its zero-call median because its total incremental work was smaller than
  startup jitter; `derived.tsv` records `NA` instead of inventing a negative
  latency.
- **fork/exec/wait:** startup-subtracted cost was 0.764 ms natively,
  43.817 ms with ptrace, and 144.196 ms with DBI. DBI was 3.29x slower than
  ptrace on this lifecycle-heavy workload.
- **compatibility:** ptrace completed every row. DBI completed five of seven
  reported workloads; KVM completed three of seven.

## CPU and memory

Median CPU is user plus system CPU milliseconds. RSS is peak KiB for the GNU
time command tree.

| Workload/mode | CPU ms | peak RSS KiB | context switches |
| --- | ---: | ---: | ---: |
| bzip2 native | 290 | 5,056 | 318 |
| bzip2 ptrace | 22,650 | 5,144 | 251,144 |
| bzip2 DBI | 3,780 | 17,860 | 6,098 |
| bzip2 KVM | 320 | 12,680 | 22 |
| Ninja native | 50 | 2,588 | 279 |
| Ninja ptrace | 7,860 | 7,660 | 86,273 |
| LevelDB native | 10 | 2,540 | 93 |
| LevelDB ptrace | 2,000 | 5,132 | 22,045 |
| SQLite native | 10 | 2,564 | 81 |
| SQLite ptrace | 1,910 | 7,668 | 21,473 |
| SQLite DBI | 770 | 17,876 | 1,457 |

Ptrace's compute overhead is predominantly system CPU and scheduler traffic:
bzip2 took 22.10 seconds of system CPU and 251K context switches. KVM kept the
same workload in one host execution context, with 20 ms system CPU and 22
context switches. DBI traded a higher roughly 17.9 MiB runtime footprint for
far lower CPU and context-switch overhead than ptrace. Tiny native commands
sometimes report zero peak RSS because they exit below GNU time's accounting
resolution; macro rows are the meaningful memory comparison.

## Capability failures

| Backend/workload | Observed boundary |
| --- | --- |
| KVM fork/exec | `fork: Function not implemented` |
| DBI Ninja | exceeded 30 seconds; process group terminated |
| KVM Ninja | `chdir ... Function not implemented` |
| DBI LevelDB | exceeded 30 seconds; process group terminated |
| KVM LevelDB | database `LOCK` creation returned `ENOENT` |
| KVM SQLite | SQLite reported `disk I/O error` |

The DBI timeouts reproduce with tiny Ninja and LevelDB work counts, so they are
compatibility stalls rather than benchmark duration. The harness's adversarial
one-second timeout test confirmed that the TERM-then-KILL cleanup leaves no
DynamoRIO guest descendants.

## Conclusion

KVM provides the strongest performance on the single-process subset it can
currently execute, reaching near-native bzip2 time and eliminating ptrace's
context-switch cost. DBI already provides substantial wins on compute and
SQLite but has high fixed startup/RSS costs and incomplete macro-application
coverage. Ptrace is much slower, especially on compute and process-heavy work,
but it remains the only backend that completed the entire requested workload
set.

These results support backend optimization priorities without claiming broad
parity: extend KVM filesystem/process coverage, fix DBI's Ninja/LevelDB stalls,
and retain ptrace as the correctness baseline while those gaps remain.
