# Arbitrary Binaries Wave 2

This experiment exercises complex servers, language runtimes, and compiler
workflows under Hermit `run`, `run --verify`, and `run --chaos`. The exact
binary versions and flags are in `metadata.txt`; full stdout/stderr for every
cell is under `results/`.

## Result matrix

| Workload | Run | Verify | Chaos | Classification |
| --- | --- | --- | --- | --- |
| nginx 1.20.1, epoll server plus curl | TIMEOUT | TIMEOUT | TIMEOUT | Loopback client connect hang, existing #18 |
| Redis 6.2.20 server plus redis-cli | TIMEOUT | TIMEOUT | TIMEOUT | Server becomes ready; loopback client connect hangs, existing #18 |
| SQLite recursive in-memory query | PASS | PASS | PASS | Deterministic |
| System Python 3.9, four threads | PASS | PASS | PASS | Deterministic |
| Meta Python 3.12 launcher, same threads | TIMEOUT | TIMEOUT | TIMEOUT | Unsupported `CLONE_VFORK`, existing #15 |
| OpenJDK 8, prebuilt JAR with four threads | PASS | PASS | PASS | Deterministic |
| Node 16, four `worker_threads` | PASS | PASS | TIMEOUT | Chaos starvation with preemption disabled, existing #20 |
| GCC compile plus four-pthread executable | PASS | PASS | PASS | Deterministic |
| Direct rustc compile plus four-thread executable | PASS | PASS | PASS | Deterministic |

`PASS` requires exit code zero and the expected workload marker. `TIMEOUT`
means the outer bounded runner returned 124. Verify comparisons for Java and
Node were allowed to finish beyond the ordinary 30-second workload limit;
Java passed in 13.4 seconds and Node passed after a 43.7-second comparison of
roughly 300,000 scheduler messages per run.

## Failure analysis

### Existing: blocking loopback connect (#18)

Nginx reaches the epoll event loop in supported single-process mode. Redis logs
`Ready to accept connections`. Their in-container curl and redis-cli clients
then fail to complete the first loopback connection. All three modes time out.
This is the existing blocking-connect class, not a server-startup failure.

- https://github.com/rrnewton/hermit/issues/18#issuecomment-5040770206

### Existing: Meta `CLONE_VFORK` launcher (#15)

`/usr/local/bin/python3` logs Hermit's unsupported `clone(CLONE_VFORK)` error
and stalls. `/usr/bin/python3` runs the identical four-thread workload in all
three modes, isolating the result from Python threading itself.

- https://github.com/rrnewton/hermit/issues/15#issuecomment-5040772852

### Existing: chaos starvation (#20)

Node worker threads pass ordinary run and verify, but default chaos with seed 1
and timer preemption disabled times out after 60 seconds. Repeating the same
case with `--sched-heuristic=random` passes in 4.6 seconds; see
`diagnostics.tsv`. This matches #20's documented fairness workaround.

- https://github.com/rrnewton/hermit/issues/20#issuecomment-5040774350

No crash, unclassified missing syscall, or new nondeterminism class was found,
so no duplicate issue was opened. The final isolated Java runtime passes
verify; an earlier diagnostic that built the JAR inside Hermit was discarded
because it mixed `javac`/`jar` process behavior into the requested `java -jar`
runtime test.

The first matrix runner used GNU `timeout` directly. Failed probes left 18
Hermit roots and 75 total processes alive, matching the stopped-process cleanup
problem already described by #15 and #19. They were terminated after capture.
The checked-in runner now starts each timeout in its own session and explicitly
kills the remaining process group after exit 124/137. The measured leak was
added to #15: https://github.com/rrnewton/hermit/issues/15#issuecomment-5040794640

## Reproduction

From the Hermit repository root:

```bash
TIMEOUT_SECONDS=30 FAILURE_TIMEOUT_SECONDS=10 \
  VERIFY_TIMEOUT_SECONDS=180 NODE_CHAOS_TIMEOUT_SECONDS=60 \
  experiments/arbitrary-binaries-wave2_20260721/run_matrix.sh
```

The runner discovers the direct rustc toolchain with `rustup which rustc`, uses
a fresh guest `/tmp` for every Hermit invocation, and exposes only `fixtures/`
at `/tmp/wave2`. Nginx uses single-process mode because ordinary packaged
worker startup calls `initgroups`, which user namespaces deny. Redis and nginx
use Hermit's isolated loopback network; no host or external network is used.

The raw table is `results.tsv`. `diagnostics.tsv` records the additional Node
random-scheduler run. `fixtures/` contains all source/config inputs and the
prebuilt Java JAR; `run_matrix.sh` contains the exact per-workload commands.
