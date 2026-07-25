# Criterion marginal syscall benchmark

This experiment measures the cost of one additional syscall under native Linux,
gVisor, and the direct Reverie instrumentation harnesses. It measures raw
interception overhead rather than Hermit/Detcore syscall policy.

## Method

`fixtures/syscall_server.c` issues raw Linux syscalls so libc and the
`clock_gettime` vDSO cannot bypass interception. It supports:

- `getpid`
- one-byte `read` from `/dev/null`
- one-byte `write` to `/dev/null`
- `clock_gettime(CLOCK_MONOTONIC)`

The host benchmark starts one persistent helper for each backend/syscall row.
Criterion's linear sampling passes an iteration count to `iter_custom`; that
count becomes the guest syscall count in one request. Backend startup is
therefore outside the timed region. The control round trip is a constant
intercept, while Criterion's linear-regression slope estimates nanoseconds per
additional syscall. Each estimate uses a 95% confidence interval and 50,000
bootstrap resamples by default.

For the requested sanity anchors, every backend also runs exactly 1,000,
10,000, 100,000, and 1,000,000 raw `getpid` calls. Those single observations
are written to `fixed-counts.tsv`; the Criterion slopes, not the anchors, are
the statistically rigorous results.

Reverie KVM currently supports neither the persistent stdio protocol nor an
AF_UNIX control connection. Its measurements execute one helper per sample,
add a fixed 1,000-call floor, scale Criterion iterations by 1,000 calls, and
divide the fitted slope by 1,000. Process startup and the fixed floor remain in
the regression intercept.

## Backends

| Result name | Direct command |
| --- | --- |
| `native` | syscall helper |
| `gvisor-systrap` | `runsc --platform=systrap do` |
| `gvisor-kvm` | `runsc --platform=kvm do` |
| `reverie-ptrace` | Reverie `counter2` |
| `reverie-dbi` | DynamoRIO `drrun` plus the Reverie DBI counter client |
| `reverie-kvm` | `reverie-kvm-counter` |
| `reverie-sabre` | `riptrace` plus the SaBRe loader and plugin |

These are instrumentation harnesses, not `hermit run`. That distinction keeps
Detcore emulation and scheduling out of the marginal trap-cost comparison.

## Relationship to gVisor

The gVisor side invokes the normal `runsc do` path implemented by
`runsc/cmd/do.go` and selects the platform through the flag defined in
`runsc/config/config.go`. The measured implementations are registered in
`pkg/sentry/platform/systrap/systrap.go` and
`pkg/sentry/platform/kvm/kvm.go`.

The benchmark treats those implementations as black-box execution backends,
just as it treats each Reverie counter harness. It does not copy gVisor's
platform, syscall, or timing code. The shared idea is only to measure a raw
syscall loop inside an already-started guest. The control protocol and
Criterion regression are Hermit experiment code; Reverie KVM's one-shot
fallback deliberately diverges because that guest cannot run either persistent
control transport yet.

## Prerequisites

Set these variables when artifacts are not at the local development defaults:

| Variable | Artifact |
| --- | --- |
| `RUNSC_BIN` | gVisor `runsc` |
| `COUNTER2` | Reverie ptrace counter |
| `DRRUN` | DynamoRIO `drrun` |
| `DBI_CLIENT` | `libreverie_dbi_client.so` |
| `KVM_COUNTER` | Reverie KVM counter |
| `RIPTRACE` | Reverie SaBRe runner |
| `RIPTRACE_PLUGIN` | `libriptrace_plugin.so` |
| `SABRE` | SaBRe loader |

gVisor KVM and Reverie KVM also require accessible `/dev/kvm`.

## Run

Choose an idle logical CPU and run:

```bash
SYSCALL_BENCH_CPU=305 ./experiments/criterion-syscall/run.sh
```

The runner uses `with-proxy` for Cargo dependency access. It writes:

- Criterion HTML to `target/criterion/report/index.html`
- raw Criterion JSON below `target/criterion/`
- backend diagnostics to `target/criterion/backend-logs/`
- capability results to `target/criterion/capabilities.tsv`
- exact getpid anchors to `target/criterion/fixed-counts.tsv`
- Markdown, TSV, and SVG comparisons to `results/latest/`

The result directory can be passed as the first argument. Both output trees are
ignored by Git.

## Configuration

The defaults are 20 samples, a 2-second warmup, 5 seconds of measurement, and
50,000 bootstrap resamples per row. Override them with:

- `SYSCALL_BENCH_SAMPLE_SIZE`
- `SYSCALL_BENCH_WARMUP_SECS`
- `SYSCALL_BENCH_MEASUREMENT_SECS`
- `SYSCALL_BENCH_RESAMPLES`
- `SYSCALL_BENCH_TIMEOUT_SECS`
- `SYSCALL_BENCH_FIXED_COUNTS`
- `SYSCALL_BENCH_ONESHOT_MIN_CALLS`
- `SYSCALL_BENCH_ONESHOT_SCALE`

Comma-separated `SYSCALL_BENCH_BACKENDS` and
`SYSCALL_BENCH_SYSCALLS` select subsets. Set
`SYSCALL_BENCH_REQUIRE_ALL=true` to fail if any selected row is unavailable.

For a quick smoke check:

```bash
SYSCALL_BENCH_CPU=305 \
SYSCALL_BENCH_BACKENDS=native,gvisor-systrap \
SYSCALL_BENCH_SYSCALLS=getpid \
SYSCALL_BENCH_FIXED_COUNTS=1000 \
SYSCALL_BENCH_SAMPLE_SIZE=10 \
SYSCALL_BENCH_WARMUP_SECS=0.1 \
SYSCALL_BENCH_MEASUREMENT_SECS=0.3 \
SYSCALL_BENCH_RESAMPLES=1000 \
./experiments/criterion-syscall/run.sh /tmp/syscall-smoke
```

Host load, CPU frequency policy, kernel version, backend revisions, and SMT
siblings affect these timings. Record them with any published result set.
