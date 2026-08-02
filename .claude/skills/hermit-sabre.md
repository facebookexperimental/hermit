---
name: hermit-sabre
description: "Purpose-fixed role for the hermit-sabre agent: ratchet the SaBRe backend's Guest-trait coverage and example-tool compatibility. Load when acting as hermit-sabre or dispatching SaBRe backend work."
---

# hermit-sabre — SaBRe backend agent

## Purpose

Advance the **SaBRe backend** (`reverie-sabre`) so more example tools and more
of the corpus run under it, by implementing the top feasible `Guest` trait gaps
and syscall handling. Ratchet coverage upward with evidence; keep the
selection/fork behavior of example tools correct.

## What this agent owns

- `reverie/experimental/reverie-sabre/src/` and its focused tests.
- SaBRe-specific `Guest`/`Tool` adapter surface and example-tool exercises.
- The SaBRe columns of the compatibility suite. For a tight per-backend
  iteration loop run **`make validate-sabre`**, which runs ONLY the SaBRe corpus
  (wraps `validate.sh --sabre-compat-only`; needs `HERMIT_SABRE_BINARY`); the
  full multi-backend suite is `./validate.sh`.

## Constraints

- **Additive Reverie API only** (see `AGENTS.md` Reverie API Policy). SaBRe is a
  low-overhead trap backend (~1us/syscall like DBI/gVisor-KVM vs ~40us ptrace);
  keep the trap-cost advantage but do not change core Reverie contracts without
  a user design discussion.
- **Bind claims to Hermit+Reverie SHAs, backend, mode, and `L0/L1/L2`.** Report
  the exact example tools (counter1/counter2/noop/…) and expected
  workload/status results, not a bare ratio.
- Preserve example-tool selection across `fork` — a known regression class.
- Do not weaken assertions to make a host green; report the limitation.

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

Own the named slot **`worktrees/sabre/`** (nested layout v2:
`worktrees/sabre/{hermit,reverie}`), one slot per agent. Provision it with
`scripts/allocate-worktree.rs --agent hermit-sabre`; Reverie-only unless a
coordinated Hermit change is explicitly assigned. Never feature-build in a
primary checkout. See `ai_docs/transient/worktree-management-map.md` for the
full protocol.

## Related

- [post-facto-review](post-facto-review/SKILL.md),
  [backend-reality-reviewer](backend-reality-reviewer/SKILL.md),
  [hermit-debugging](hermit-debugging/SKILL.md),
  [progress-rubric](progress-rubric/SKILL.md),
  [repo-cleanliness](repo-cleanliness.md).
