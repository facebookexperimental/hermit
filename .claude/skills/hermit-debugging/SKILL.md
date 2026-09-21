---
name: hermit-debugging
description: "Debug Hermit and Detcore nondeterminism from exact-SHA run evidence and Hermit's logs. Use when repeated guest runs disagree, --verify reports a divergence, or tests/DEBUGGING.md needs a current investigation entry."
---

# Debugging Hermit Runs

**Thesis: reach for Hermit's logs before you reach for the source.** Detcore
emits a rich, structured trace of every scheduling decision, syscall, and
virtual-time advance. Locate nondeterminism by finding the first divergent
COMMIT or DETLOG record before reading `scheduler.rs`. Read the code only once
the log has told you *where* to look.

All commands below assume the repo root and the release binary
`target/release/hermit` (use `target/debug/hermit` if that is what you built).

Outside `validate` and the E2E manifest infrastructure's official runner,
invoke every Hermit binary through the `dev-hermit` parent workspace's
`bin/safehermit` wrapper. This includes arbitrary absolute-path binaries under
`/tmp`:

```bash
../bin/safehermit ./target/release/hermit run -- ./prog
../bin/safehermit /tmp/hermit-patched run -- ./prog
```

The wrapper caps the child process's stderr. It does not cap files written by
`--log-file` or retained verify logs; those have separate guards.

## Establish what actually failed

Start from the exact failing observation. For a manifest cell, record its lane,
manifest bucket, test id, mode, and backend. For a direct `hermit run`, record
the literal guest command and the applicable mode and backend; do not invent a
manifest identity that the run does not have. Bind every observation to the
full Hermit SHA and tree, the binary path, `binary_build_sha` and binary hash
when available, `run_id`, the evidence source's timestamp,
host/kernel/toolchain, exact command, working directory, environment, timeout,
exit status, and retained result/log paths. If the binary may be stale, rebuild
it in the checkout at that SHA before attributing behavior to the source. For a
claimed regression, run the same bounded probe against a clean current-main
control.

Read existing evidence before rerunning. For a manifest run, read the run-level
`results.jsonl` first for the typed cell result. When the harness outcome names
an artifact directory, follow it for the verify report, captures, and logs. Do
not assume every non-PASS outcome has one: `HOST-INAPPLICABLE` may report only
its reason.

Keep the repository's outcomes distinct. `FAIL`, `ERROR`, `HOST-INAPPLICABLE`,
`never-measured`, `measured-no-verdict`, a missing row, and an incomplete run do
not mean the same thing, and none may be relabeled PASS. A failure under
contention is a real defect: record the observed width/load, rerun the same cell
alone to isolate the mechanism, and keep both results. A solo pass does not
erase the contended failure or make it unclassifiable.

Use the project's existing vocabulary and exact identifiers. If the repository
has no established term for an observation, describe it in plain language; do
not coin a new class or mechanism name.

## Maintain `tests/DEBUGGING.md` as current state

`tests/DEBUGGING.md` is the human-readable index of investigations that are
active now. If the file does not yet exist, create it only when there is a
current misbehavior to record and add its required `support-data` entry to
`tests/e2e/manifests/inventory/test-files.json` in the same change.

- Use one H1 heading for every current manifest `bucket` from
  `tests/e2e/manifests/*.yaml`, spelled exactly like its `bucket` field and kept
  in lexical order. H1s remain even when their bucket has no current failures;
  add or remove them when the manifest bucket set changes.
- Under the owning bucket, use one H2 heading for each test that is
  **currently** misbehaving. Use the manifest test id verbatim. If several
  mode/backend cells fail for that test, keep them in the same H2 and identify
  each complete cell separately.
- Add evidence as timestamped H3 entries. Preserve the timestamp reported by
  the evidence source and use the exact per-run outcome (`PASS`, `FAIL`,
  `ERROR`, `HOST-INAPPLICABLE`, or a typed verdict such as `no_result`); do not
  invent a replacement label. For a repeated sample that contains both PASS and
  FAIL observations, use the project's existing `flaky` classification and
  record the PASS and FAIL counts separately. Record the full SHA and tree,
  cell, command and exit, result class, first divergent coordinates when
  present, contention/host facts, retained evidence paths, current hypothesis,
  what the observation ruled out, and the next discriminating check. Link large
  logs; do not paste them into the journal.
- The journal describes the present investigation, not its archive. Git history
  preserves removed prose.

Use this shape:

```markdown
# system-utils

## system-utils/mktemp-name

### 2026-08-26T12:42:10-07:00 — portable / verify / ptrace — FAIL

- Hermit: `<40-hex SHA>`; tree: `<40-hex tree>`; binary: `<path, binary_build_sha, sha256>`
- Command: `<literal command>`; exit: `<status>`; contention: `<jobs/load>`
- Evidence: `<run handle, result directory, verify report, log>`
- Observed: `<first divergent record/turn/syscall or last progress>`
- Current explanation: `<what the evidence supports, in project vocabulary>`
- Ruled out / next: `<negative result>; <next check that can distinguish causes>`
```

**Deletion is mandatory.** Remove the entire H2 as soon as every cell named in
it is green and non-flaky. Do not leave a `resolved` section, tombstone, or old
hypothesis in the live journal: stale failures read as current and are worse
than no journal. A retry that fails and then passes is evidence of flakiness,
not grounds for deletion. For every cell named in the H2, require an exact-head
typed result that exercises its declared mode/backend contract. A missing or
untyped result, zero selected or executed tests, a stale binary, or a probe of a
different mode/backend cannot justify deletion. If the product gap that keeps a
cell disabled still exists, the test is still misbehaving and its H2 stays.

For a previously flaky determinism cell, also require the same source and
binary identity to complete a fixed-tree repeat sample with no FAIL, ERROR,
`no_result`, timeout, or other non-PASS observation: 20 successful repetitions
unless a stricter test-specific policy applies. If any cell named in the H2
remains red, flaky, unobserved, or `no_result`, keep the H2 and update its newest
timestamped entry.

While the journal exists, retain every current bucket H1 even when that bucket
has no H2. When the last H2 in the whole file is deleted, delete
`tests/DEBUGGING.md` and its `support-data` entry from
`tests/e2e/manifests/inventory/test-files.json` in the same change. Do not keep
an all-empty journal: it would imply that a current investigation exists.

## 0. First move, always

```bash
# Separate hermit's log (stderr) from the guest's own output (stdout):
target/release/hermit --log info run -- <program> [args...] 2>/tmp/h.log
#   ^ global flag, BEFORE the subcommand.   guest stdout stays on your terminal
wc -l /tmp/h.log      # a trivial `echo hello` produces ~350 INFO lines
```

Do **not** interleave hermit logs into guest stdout. Either redirect stderr as
above, or use the dedicated flag:

```bash
target/release/hermit --log info --log-file /tmp/h.log run -- <program>
```

`--log-file` (env `HERMIT_LOG_FILE`) writes the trace to a file and leaves the
guest's stdout/stderr untouched — the cleanest way to keep the two streams
apart.

### Log levels (`-l/--log`, env `HERMIT_LOG`)

| Level | Use it for |
| --- | --- |
| `error` / `warn` | Quiet; only when you want the guest to run near-normally (used for QEMU boots). |
| `info` | **Default debugging level.** Every COMMIT (scheduling turn), every DETLOG syscall, every virtual-time advance. Start here. |
| `debug` | Adds `reverie_ptrace::task` events, per-step scheduler internals, `tracee` lines. ~2-3x the volume. Use when INFO isn't enough. |
| `trace` | Everything, including `[sched-step*]` micro-steps and quiescence waits. Very large; scope it (see per-target filtering). |

Per-target filtering uses `tracing`/`RUST_LOG` syntax, so you can crank up one
module without drowning in the rest:

```bash
HERMIT_LOG='info,detcore::scheduler=trace' target/release/hermit run -- <program> 2>/tmp/h.log
```

## 1. How to read the trace

Every line is `TIMESTAMP LEVEL target: message`. The `target` tells you the
subsystem: `detcore`, `detcore::scheduler`, `detcore::scheduler::runqueue`,
`detcore::syscalls::files`, `detcore::tool_global`, `detcore::tool_local`,
`reverie_ptrace::task`. Grep by target to isolate a subsystem.

The two message classes that matter most both live in the deterministic trace:

**COMMIT lines** — one per scheduler *turn*. This is the serialized schedule.

```
[sched-step5] >>>>>>>
 COMMIT turn 0, dettid 3 using resources {ParentContinue { parent: DetPid(3), child: DetPid(3) }: W}, on previously committed 1_640_995_199.000_000_000s
 COMMIT turn 1, dettid 3 using resources {MemAddrSpace(DetPid(3)): RW}, on previously committed 1_640_995_199.000_500_000s
```

Read it as: *turn N, thread `dettid`, acquired these resources (R/W), at this
committed virtual time.* The **sequence of `(turn, dettid)` pairs is the
schedule** — the single most important thing to diff between two runs.

**DETLOG lines** — deterministic facts: syscalls, their results, RNG seeds, etc.

```
DETLOG [syscall][detcore, dtid 3] inbound syscall: openat(-100, ... "/etc/ld.so.cache", OFlag(O_CLOEXEC)) = ?
DETLOG [syscall][detcore, dtid 3] finish syscall #3: openat(...) = Ok(3)
DETLOG SCHEDRAND: seeding scheduler runqueue with seed 0
DETLOG USER RAND: seeding PRNG for root thread with seed 0
```

`inbound syscall: ... = ?` is interception; `finish syscall #N: ... = Ok(..)` is
the sanitized result handed back to the guest. A syscall that appears inbound
but whose result looks like passthrough of a host value is a determinism
suspect.

**Virtual time (DetTime / LogicalTime).** Time in hermit is *logical*, not wall
clock. `detcore-model/src/time.rs` defines:

- `LogicalTime(u64)` — absolute nanoseconds since a fixed epoch
  (`starting_micros`, default `1640995199000000` = 2021-12-31T23:59:59). This is
  why guest timestamps are identical across runs.
- `DetTime { syscalls, rcbs, nondet_instrs, starting_micros, multiplier }` —
  virtual time is a deterministic *function of work done*, not of the host
  clock. It advances by counting **syscalls**, **RCBs** (retired conditional
  branches, from the PMU — the preemption clock), and **nondet_instrs**
  (`rdtsc`/`cpuid`).

You see it advance in the trace:

```
[dtid 3] inbound rdtsc, new logical time: DetTime { syscalls: 1, rcbs: 49, nondet_instrs: 1, starting_micros: 1640995199000000, multiplier: 1.0 }
```

and summarized at shutdown:

```
Internally, the hermit scheduler ran 34 turns, recorded 0 events, replayed 0 events (0 desynced)
Final virtual global (cpu) time: 1_640_995_199.019_160_055s
```

If the RCB counts for the same logical point differ between two runs, the guest
executed a different number of branches — a real divergence, not a clock
artifact.

### Quick grep cookbook

```bash
grep ' COMMIT turn '            /tmp/h.log   # the schedule (turn, dettid) sequence
grep ' DETLOG '                 /tmp/h.log   # deterministic facts
grep 'inbound syscall'          /tmp/h.log   # syscalls intercepted, in order
grep 'new logical time'         /tmp/h.log   # virtual-time advances (DetTime)
grep -iE 'park|unpark|go-ahead|New thread|run queue|quiescen' /tmp/h.log  # thread lifecycle
grep -oE 'detcore[a-z_:]*'      /tmp/h.log | sort | uniq -c | sort -rn    # subsystem histogram
```

## 2. Finding a nondeterminism / divergence point

When `hermit run --strict --verify --verify-strict` reports
"nondeterministic", Hermit already ran twice and compared exact
exit/stdout/stderr plus INFO events under `BitwiseInfoV1`. To localize the
divergence yourself, capture two runs and use the **built-in log differ**. Its
more aggressive normalization of hex pointers, tmp paths, `/proc/<pid>/`, and
elapsed-time fields makes it a diagnostic aid only. The final fix must
produce `bitwise_parity: true` through `--verify-json`, with nonzero compared
INFO-message counts. Apply the same requirement to KVM; backend capability or
output/status repeatability alone is not evidence that a particular KVM cell
reached canonical full-observation parity.

```bash
target/release/hermit --log info run -- <program> 2>/tmp/a.log
target/release/hermit --log info run -- <program> 2>/tmp/b.log
target/release/hermit log-diff /tmp/a.log /tmp/b.log # compares COMMIT + DETLOG only
```

Useful `log-diff` flags (`detcore/src/logdiff.rs`):

| Flag | Effect |
| --- | --- |
| `--unsafe-strip-lines` | **Non-parity diagnostic only.** Erases timestamps and syscall values; using it to make a failing parity diff pass is cheating. |
| `--syscall-history <N>` | Print the N completed syscalls *before* each divergence — the context that tells you what led up to it. |
| `--ignore-lines <substr>` | Drop lines containing a substring before comparing (repeatable). |
| `--skip-commit` / `--skip-detlog` | Compare only DETLOG, or only COMMIT, to tell a *scheduling* divergence from a *syscall/data* divergence. |
| `--include-detlogs syscall,syscallresult,other` | Narrow which DETLOG classes count. |
| `--limit 0` | Don't elide after 20 diffs; show all. |

**Interpretation:** the *first* divergence is the one that matters; everything
after it is downstream noise. If the first diff is a **COMMIT** line
(`(turn, dettid)` differs), the *schedule* diverged — a thread-interleaving
problem. If COMMITs match but a **DETLOG** line differs, the schedule is stable
but a syscall returned different data — an unvirtualized source.

## 3. Common root causes, and their log signatures

| Symptom in the log | Likely cause | Where to look |
| --- | --- | --- |
| First diff is a `COMMIT` line — different `(turn, dettid)` order between runs | **Thread-interleaving nondeterminism.** Often from `--no-sequentialize-threads`, or a futex/blocking-IO race. | `detcore/src/scheduler.rs`; check for relaxation flags. |
| DETLOG syscall result differs; value looks like a live host reading (time, meminfo, rand) | **Unvirtualized time / entropy source** falling through to the host. | `detcore/src/time.rs`, the relevant `detcore/src/syscalls/` handler. |
| `cpuid` in the trace and behavior varies by host | **CPUID leaking real hardware.** Try `--no-virtualize-cpuid` to confirm it's CPUID-related; the host may lack CPUID faulting. | `detcore/src/cpuid.rs`. |

## 4. Debugging-specific CLI flags

Global (before the subcommand): `--log`, `--log-file`, `--backend <ptrace|dbt|kvm>`.

On `run` (see `hermit run --help`), the internal/debug flags:

- `--stacktrace-event <index[,path]>` — print the guest stack at a given
  schedule event; pairs with record/replay.
- `--preemption-stacktrace[-log-file <f>]` — dump a stack at each preemption
  (chaos mode).
- `--debug-externalize-sockets` — treat all sockets as external/nondeterministic
  to isolate socket-driven nondeterminism.
- `--detlog-heap` / `--detlog-stack` — log hashes of heap/stack maps for
  memory-determinism checking.
- `--stop-after-turn <N>` / `--stop-after-iter <N>` — halt after a scheduler
  turn/loop iteration (requires `--sequentialize-threads`) to bisect a schedule.
- `--imprecise-timers` / RCB-count knobs — change how logical time is derived
  when the PMU is unavailable or noisy.

Higher-level analysis subcommands: `hermit log-diff` (above),
`hermit analyze` (analyze passing vs failing runs), and `hermit bisect`
(`--good <schedule> --bad <schedule>` to localize a race between two recorded
schedules).

## 5. Report validation levels precisely

- **L1:** `hermit run --strict` exits 0 for one fail-closed execution. One run
  does not establish repeatability.
- **L2:** two strict runs on the same backend have identical exit status,
  stdout, stderr, and canonical INFO events. Current evidence requires
  `hermit run --strict --verify --verify-strict --verify-json <path> -- ...`,
  JSON `bitwise_parity: true`, and nonzero compared INFO-message counts. L2 is
  not cross-backend parity; `Parity vs ptrace` is a separate axis.
- **L3:** L2 with `--detlog-heap --detlog-stack`, so heap and stack hashes are
  also compared between the two runs. L2 can pass while L3 fails.
- **L4:** run the stated L2 or L3 command 20 times and require 20/20 successful
  repetitions. State whether L2 or L3 was repeated; an L4 record that omits
  that fact is ambiguous.

The evidence accepted for L2 changed on 2026-08-05 in
https://github.com/rrnewton/hermit/commit/806b6766551dd23b6549e8d56b76419164665bf7,
and remaining documentation was corrected on 2026-08-12 in
https://github.com/rrnewton/hermit/commit/d2790d99d7f96363283ca9287ed79217d3503a4f.
Pre-2026-08-05 L2 records attest the old bare-`--verify` definition unless their
retained evidence independently satisfies the current requirements. KVM became
eligible for the same L2 evidence on 2026-08-25 in
https://github.com/rrnewton/hermit/commit/50794c05b1a020240759d6afeaa9f14ee5ba8f29;
that did not make cross-backend parity part of L2.

## 6. Source-code map (read *after* the log points you here)

- `detcore/src/scheduler.rs` — the sched loop; `[scheduler]`, `[sched-step*]`,
  and COMMIT emission (`info!`/`debug!`/`trace!`). The COMMIT point is step 4.
- `detcore/src/logdiff.rs` — the log comparator: `strip_log_entry`
  normalization, `is_commit`/`is_detlog`, `LogComparisonMode`, `LogDiffOpts`.
- `detcore-model/src/time.rs` — `LogicalTime`, `DetTime`, `GlobalTime`, and the
  RCB↔nanosecond conversions.
- `detcore/src/syscalls/` — per-syscall handlers (`files.rs`, etc.).
- `detcore/src/cpuid.rs`, `detcore/src/time.rs` — CPUID and time virtualization.
- `detcore/src/tool_local.rs` / `tool_global.rs` — per-task events vs shared
  deterministic state (they talk over RPC).
- `docs/Developers/Architecture.md` — architecture overview.

## If in a `dev-hermit` multi-worktree space

This section is optional. The Hermit debugging method above does not assume the
parent repository or its coordination tools.

- For a red validation receipt or failed DAG node, start with
  [`ci-debugging`](../ci-debugging/SKILL.md). Use this skill when that
  investigation reaches the behavior of a guest run.
- Use the timestamped validate LEDGER and `ci-hub validate-status` for run-level
  evidence. Use `ci/compat-envelope/cells.json` for the cell's checked-in
  status, `last_tested`, measurement state, and pressure/validate observations;
  its optional `projection.refreshed_at` and `rows_read` fields say whether the
  projection is current. If the projection block is absent, freshness is
  unknown. Join run-level and per-cell evidence by `run_id`, full cell identity,
  source SHA, and binary identity.
- Journal pruning does not authorize changing
  `ci-hub/validate/flaky-cells.json`. That registry is keyed by validate DAG-node
  names rather than manifest cell identities, and it defines how to add entries
  but not how to remove them. If an entry appears stale, record the exact node
  mapping and evidence in a separate TaskGraph task. Never remove a registry
  entry because one `tests/DEBUGGING.md` H2 became green.
- Use TaskGraph for durable coordination. On Meta devservers, prefix commands
  that need the public internet with `with-proxy`.
