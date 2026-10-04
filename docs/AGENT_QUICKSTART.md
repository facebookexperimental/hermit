# Hermit quick start for coding agents

Use this workflow when reproducing a flaky test or collecting deterministic
execution evidence. It is intentionally conservative: build tools stay outside
Hermit, every ad-hoc Hermit execution is bounded, and a green terminal message
is not accepted without its machine-readable evidence.

## 1. Freeze the test command

Build the test natively. Do not put Cargo, Buck, a compiler, or a build daemon
inside Hermit. For this repository, for example:

```bash
cargo build --release -p hermit --bin hermit
```

For a Buck test, let Buck build the target and discover its executable, then run
that executable directly under Hermit with the exact test arguments. Record all
of these inputs before searching schedules:

- the Hermit commit and binary SHA-256;
- the guest executable SHA-256 and immutable input files;
- the guest's byte-exact argument vector;
- an explicit guest working directory;
- `--base-env=minimal` plus every required `--env NAME=VALUE`;
- the backend, epoch, random seed, scheduler seed, and all scheduling flags.

The kernel places arguments and environment on the initial stack, so changing
either can change execution before the guest reaches `main`. Paths and mutable
files are inputs too. Use the same absolute paths when reproducing a run.

## 2. Start with one short, bounded run

First time the shortest native test. Then run one seed under Hermit's ptrace
backend before starting a batch. Agents working in the `dev-hermit` workspace
must route every ad-hoc invocation through the parent's safety wrapper, even
when the Hermit binary is in a worktree or under `/tmp`:

```bash
repo=$(git rev-parse --show-toplevel)
hermit_bin="$repo/target/release/hermit"
safehermit=/home/newton/work/dev-hermit/bin/safehermit
scratch=$(mktemp -d /tmp/hermit-agent.XXXXXX)
epoch=$(date -u +%Y-%m-%dT%H:%M:%SZ)

guest=/absolute/path/to/test-binary
guest_cwd=/absolute/path/to/test-working-directory
report="$scratch/verify.json"

"$safehermit" --sh-deadline=180 --sh-max-log-bytes=16777216 \
  "$hermit_bin" --backend=ptrace run --timeout=60 --strict --network=local \
  --base-env=minimal --env LANG=C.UTF-8 --workdir="$guest_cwd" \
  --epoch="$epoch" --seed=0 --rng-seed=0 --sched-seed=0 \
  --verify --verify-strict --verify-json="$report" -- \
  "$guest" ARGUMENTS...
```

Capture `epoch` in the experiment metadata and reuse that literal value; do not
run `date` again for a reproducer. Current Hermit defaults to a fixed
`2026-01-01T00:00:00Z` epoch, so choosing and recording a current value is the
way to test a realistic clock without making it an uncontrolled input.

The wrapper's `bound.*=APPLIED` lines identify its active wall-time, log-size,
disk, and memory guards. `run --verify` can execute two sequential attempts, so
the 180-second outer deadline leaves margin around both 60-second guest bounds.
A timeout, launch refusal, missing report, or `no_result` is infrastructure
evidence, never a passing test.

## 3. Verify the evidence, not the banner

For L2 canonical parity on the ptrace backend, require all of these fields:

```bash
jq -e '
  .verdict == "matched" and
  .bitwise_parity == true and
  .comparison.strictness == "canonical" and
  .comparison.virtualize_time == true and
  .compared_log_messages.left > 0 and
  .compared_log_messages.right > 0
' "$report"
```

Also classify the guest result with the test's real oracle. When reproducing a
known nonzero exit, add `--verify-allow=failure`; it requires both executions to
fail. Reserve `--verify-allow=both` for schedule exploration where a separate
test oracle classifies each result. Report the backend, log level, and
determinism relaxations with every result. Plain `--verify` uses a lossy
comparator and does not establish L2.

## 4. Search schedules without changing other inputs

Vary only the scheduler seed first:

```bash
"$safehermit" --sh-deadline=90 --sh-max-log-bytes=16777216 \
  "$hermit_bin" --backend=ptrace run --timeout=60 --strict --network=local \
  --base-env=minimal --env LANG=C.UTF-8 --workdir="$guest_cwd" \
  --epoch="$epoch" --seed=0 --rng-seed=0 --sched-seed=17 \
  --chaos --sched-heuristic=random -- \
  "$guest" ARGUMENTS...
```

A seed names a run only together with the Hermit binary, guest binary, argv,
environment, working directory, epoch, inputs, and flags. Save the full command
for both a passing and a failing seed. If a host cannot use performance
counters, `--max-timeslice=disabled` still explores scheduling at syscall and
thread events, but it is a smaller coverage envelope and must be reported.

After one seed finishes and the oracle is correct, run a small independent
batch, initially 4-8 workers with a unique output directory per seed. Measure
resident memory, disk growth, and slowdown before increasing concurrency.
Available CPU count is not a safe initial worker count; Hermit ptrace runs can
be much slower than the native test.

## 5. Know the current network and replay boundary

`hermit run` defaults to `--network=local`, an isolated loopback interface.
Local client/server traffic inside the guest namespace can therefore work
without exposing external traffic. `--network=host` exposes the host network,
weakens reproducibility, and is rejected with `--strict`.

Current `hermit record start` is experimental whole-execution record/replay,
not a network-only recorder. It observes the real host clock, and its
compatibility `--strict` flag is accepted but ignored. This command checks
replay fidelity for one recording under the canonical comparison:

```bash
"$safehermit" --sh-deadline=180 --sh-max-log-bytes=16777216 \
  "$hermit_bin" --backend=ptrace record start --record-timeout=60 \
  --base-env=minimal --env LANG=C.UTF-8 --workdir="$guest_cwd" \
  --verify --verify-strict --verify-json="$scratch/replay.json" -- \
  "$guest" ARGUMENTS...
```

With `--verify`, Hermit uses a temporary recording and removes it afterward;
`--data-dir` does not select storage for this path. `--record-timeout=60` bounds
only the recording. Replay has no separate inner timeout, so the 180-second
`safehermit` deadline is its remaining hard bound. A matched result means the
replay matched the recording. It does not mean two fresh recordings match, and
it does not currently provide a way to replay only network traffic while
varying the schedule. Avoid external services in deterministic `run`
experiments unless their traffic is itself a deliberately documented, unsafe
input.

## 6. Keep only intentional outputs

Build products stay in the designated worktree. Put a curated experiment
manifest, reproducer commands, small evidence files, and final report under
`/home/newton/work/dev-hermit/experiments/<campaign>/` when they are intended
project outputs. Put other scratch data in a uniquely named directory under
`/tmp` or `~/temp`, then delete it after extracting the evidence. Do not create
extra checkouts or dump logs directly under `~/work`.

Before handing off, confirm the experiment records hashes and exact inputs,
stop all child processes, remove transient files, and retire the registered
worktree through `wrkslots`. For a verification divergence, classify the first
differing record using [Divergence classes](DIVERGENCE_CLASSES.md) before
forming a root-cause claim.
