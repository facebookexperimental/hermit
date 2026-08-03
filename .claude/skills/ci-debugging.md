---
name: ci-debugging
description: "Debug and fix red Hermit/Reverie CI with TIGHT, FOCUSED iteration: reproduce and iterate on the single failing shard locally via ci/run-node.sh (or validate.sh --only) instead of re-running the whole 40-50 min DAG per cycle. Use whenever a CI lane is red and you are about to push a fix — especially before pushing more than once."
---

# Debugging CI with tight, focused iteration

> **The failure mode this skill exists to prevent:** a `push -> wait 34-50 min
> full-DAG -> read one red shard -> push again` loop, often with cancelled
> in-flight runs. That loop burns hours of wall-clock for minutes of signal. The
> CI lanes are already factored into ~45 independent DAG nodes; iterate on the
> ONE that failed.**

The portable and privileged lanes are defined as DAG files (`ci/dag/portable.json`,
`ci/dag/privileged.json`). Each node (`group.job`, e.g. `test.sabre_examples`,
`e2e.manifest_backend_parity_c`) is an independently boxed step with its exact
command. `validate.sh` and GitHub Actions both consume these same files, so a node
run locally is the same command CI runs.

## 1. Iterate on the SINGLE failing shard locally

When a lane is red, find the failing node(s), then run just those against your
already-built tree — dependencies are assumed present, so this is seconds-to-minutes,
not the whole lane:

```bash
# Identify which job(s) failed in the red run:
with-proxy gh run view <run-id> -R rrnewton/hermit --json jobs \
  | jq -r '.jobs[] | select(.conclusion=="failure") | .name'

# Build the tree ONCE, then loop on the failing node only:
ci/run-dag.sh portable                          # one full build (or cargo build --workspace)
ci/run-node.sh portable test.sabre_examples     # re-run just this shard, no deps
ci/run-node.sh portable e2e.manifest_backend_parity_c,test.dbi_parity   # comma-list
```

`ci/run-node.sh <lane> <group.job>[,...]` runs exactly the listed nodes' commands
from `ci/dag/<lane>.json`, in order, stopping at the first failure. It needs `jq`
and the `safe-ci-dag-runner` (already in `agent-utils/`).

## 2. Use `validate.sh --only` as the first-class entrypoint

`validate.sh --only <lane> <group.job>[,...]` is the canonical wrapper — it
delegates straight to `ci/run-node.sh` and skips the full harness:

```bash
./validate.sh --only portable test.sabre_examples
```

This exists precisely so single-shard iteration is one command. Do NOT reach for
`./validate.sh --portable-only` (whole lane, ~34-50 min) while iterating on one
shard.

## 3. Batch changes; do not cancel runs; rerun only what failed

- **Batch** independent fixes into one commit -> one CI cycle. N separate fixes as
  N pushes is N full cycles.
- **Never push over an in-flight run you care about.** A new push cancels the
  previous run, wasting the runner-minutes already spent and yielding zero signal.
  Let a run you're waiting on finish.
- **Re-test only the failed job** on GitHub when no code change is needed (e.g.
  confirming a flake cleared) — do not re-fire the whole workflow:

  ```bash
  with-proxy gh run rerun <run-id> -R rrnewton/hermit --failed
  with-proxy gh run rerun -R rrnewton/hermit --job <job-id>
  ```

## 4. ~50% of failures are locally reproducible — only the env-only half needs GitHub

Before assuming a fix requires a GitHub round-trip, classify the failure:

- **Locally reproducible (iterate with `--only` / `run-node.sh`):** unit/integration
  tests, build/link errors, manifest/inventory checks, version-provenance, most
  `cargo` and `e2e.manifest_*` node failures. This is roughly half of real red-CI
  work — do NOT round-trip to GitHub for these.
- **GitHub-runner-environment only (round-trip unavoidable):** mixed-kernel runner
  tooling (e.g. `bpftool`), hosted parallelism / core-count tuning, cross-job
  artifact packaging in the fan-out topology, runner user-namespace availability,
  load-sensitive privileged timing. Local single-shard runs CANNOT reproduce these;
  batch them and use `rerun --failed`.

Be honest about which half a given failure is in — do not blame tooling for an
env-only failure, and do not waste a 40-min cycle on a locally-reproducible one.

## 5. Read EARLY signals; do not serial-wait

- A red job usually fails early (build/lint/first test). Watch the failing job's
  live log and act on the first failure instead of waiting for the whole lane to
  finish and report.
- `merge-gate` is a re-fire placeholder that is **red-by-design until the portable
  lane completes** — it is not a diagnostic. Read the portable rollup's per-job
  results, not merge-gate.
- After the CI split, the authoritative hermit gate is
  `Regular tests (GitHub-managed portable)`; `Privileged capability and E2E tests`
  is non-blocking. Reverie's gates are `Regular tests` + `Host-dependent tests`.
  A queued/stale/cancelled check is not green — and a single serialized PMU
  privileged runner means queueing is expected, not failure.

## 6. Runner-queue contention vs a code regression

The privileged/PMU lane runs on a **single serialized (flock'd) self-hosted
runner**. During a landing burst, superseded runs are cancelled mid-build and
concurrent builds stack on that one host, so wall-clock-sensitive buckets can
**time out from contention, not correctness**. Before blaming the commit:

- A **timeout** (not an assertion failure) on the single privileged runner during
  a burst is a **load/queue artifact** until proven otherwise. Real example: the
  KVM `applications` bucket ran 38s on a quiet runner and 120s (TIMEOUT) 27s later
  on the same runner under a ~7-commit landing burst — the failing commit only
  touched an unrelated proc-fd fixture.
- Check for concurrent/cancelled runs and the **same bucket's baseline timing on a
  quiet runner** before calling it a regression.
- A timeout in a bucket **unrelated to the commit's changed category** is a strong
  contention signal.
- Fix-forward with `gh run rerun <id> --failed` on a quiet runner; escalate the
  systemic wall to **load-relative timeouts** — never weaken the correctness
  assertion to make a loaded runner green.
- Structurally: **throttle** what fires at the serialized runner. Rebasing in
  parallel is fine; firing CI at the single runner from N agents is the
  mass-parallel-drain cancellation cascade.

## 7. Common red cause vs per-PR rebase churn

When main or many PRs are red, do **not** reflexively rebase/re-run every PR.
First classify **shared-cause vs per-PR-cause**:

- Count which PRs actually touch the failing surface: `gh pr list -R rrnewton/hermit`
  plus a path grep. Example: across 224 open PRs, only 3 touched `.github/workflows/*`
  and **0** touched `run-dag.sh`/`safe-ci-dag` — so a CI-DAG fix lands **once** and
  ~222 product PRs inherit it on their normal rebase.
- A stale-pin / freshness gate flaps **every** PR red from **one** cause; one
  product pin-bump clears them all. Chasing it per-PR is O(N) waste for an O(1)
  root fix.
- Fix a shared infra/config cause **once at the root**; reserve individual rebases
  for genuine per-PR content conflicts.
- Land shared CI/infra refactors **before** a big landing sprint so the fleet
  inherits them conflict-free.

## Related

- [hermit-ci](hermit-ci.md) — the CI health & improvement role that uses this skill.
- [hermit-debugging](hermit-debugging/SKILL.md),
  [deadlock-debugging](deadlock-debugging.md),
  [determinism-regression-debugging](determinism-regression-debugging/SKILL.md) —
  for debugging the *guest* failure a red shard exposes, once you can reproduce it
  locally.
- [repo-cleanliness](repo-cleanliness.md), [hermit-coord](hermit-coord.md).
