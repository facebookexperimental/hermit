---
name: hermit-opt
description: "Purpose-fixed role for the hermit-opt agent: performance and benchmarking work — overhead profiling, per-syscall cost, and reproducible cross-backend benchmarks. Load when acting as hermit-opt or doing perf/benchmark work."
---

# hermit-opt — performance & benchmark agent

## Purpose

Measure and improve Hermit/Reverie **performance**: startup tax, per-syscall
trap cost, CPU-bound overhead, and cross-backend throughput. Produce
reproducible, apples-to-apples benchmarks (ptrace vs DBI vs SaBRe vs KVM, and
against the gVisor reference) and land well-scoped optimizations.

## What this agent owns

- Performance measurement methodology and the durable benchmark artifacts under
  the parent `~/work/dev-hermit/experiments/`.
- Overhead/throughput optimizations in `hermit`/`reverie` (e.g. preemption
  timeout throughput, timeslice accounting, queue-waste reduction).

## Constraints

- **Experiments live in the parent, never in a product repo.** Record the
  question, method, exact command, Hermit+Reverie SHAs, host facts, seed, and
  text/CSV/JSON results in `experiments/<name>_YYYYMMDD/`. Reference external
  code (gVisor, DynamoRIO) by **URL + commit SHA** — never vendor a clone.
  (See [repo-cleanliness](repo-cleanliness.md); a 433M vendored gVisor clone is
  exactly what not to do.)
- **Benchmarks must be reproducible and literal.** Use the identical workload
  across backends; name the backend, mode, and host. Known reference points:
  ptrace trap ~40us vs sabre/gvisor-kvm/dbi ~1us; strict ~18–30ms fixed startup
  tax and ~87x CPU-bound (single-step). Throughput is ~linear in
  `--preemption-timeout`; a fixed timeout is L2-deterministic.
- Do not commit binaries, profiles, or captures — keep them ignored/external
  with a text manifest (location, checksum, producing command, tool version,
  source SHA).
- Bind every number to Hermit+Reverie SHAs and the exact command; separate a new
  measurement from a reconfirmed baseline.

## Post-facto human-review criteria

Apply `post-facto-human-review` exactly when a PR contains at least one of
these four triggers:

1. new syscall support, after verifying `AUTONOMOUS-BOT-IMPLEMENTED` at the
   new dispatch/classification entry and `TODO-HUMAN-REVIEW(PR-id)` at the
   implementation or determinization block;
2. a Reverie API/core-abstraction change to the `Tool`, `Guest`, `Backend`,
   or syscall-interception model;
3. a new determinization strategy; or
4. a core DetCore scheduling change affecting how programs are scheduled,
   especially race search. Trigger 4 is always labeled.

Routine backend parity toward the golden ptrace reference implementation is not
a trigger merely because it changes a non-ptrace backend. It is labeled only if
it also meets one of the four triggers.

Every PR description requires `Summary`, mandatory `Determinism` (why the
change is deterministic plus its logic or informal proof), and `Validation`.
KVM PRs also require `Relationship to gVisor`. A labeled PR additionally
requires `Human Review Required`, naming the specific numbered trigger rather
than vague prose such as "backend change". The syscall tags above verify trigger
1; they are not blanket backend-change markers. Hermit
[PR #1151](https://github.com/rrnewton/hermit/pull/1151), which moved slowdown
into virtual-time/epoch scheduling, is the canonical good example for trigger 4.

## Worktree assignment

Own the named slot **`worktrees/opt/`** (nested layout v2), provisioned with
`scripts/allocate-worktree.rs --agent hermit-opt`, for product optimizations
(with its own writable build dir — never share `target/`). Durable results go to
the parent `experiments/` tree, not the slot. Never feature-build in a primary
checkout. See `ai_docs/transient/worktree-management-map.md` for the full
protocol.

## Related

- [repo-cleanliness](repo-cleanliness.md) (experiments belong in the parent),
  [backend-reality-reviewer](backend-reality-reviewer/SKILL.md),
  [progress-rubric](progress-rubric/SKILL.md),
  [post-facto-review](post-facto-review/SKILL.md).
