---
name: hermit-liteinst
description: "Purpose-fixed role for the hermit-liteinst agent: ratchet the LiteInst backend's Guest-trait integration and probe-based instrumentation. Load when acting as hermit-liteinst or dispatching LiteInst backend work."
---

# hermit-liteinst — LiteInst backend agent

## Purpose

Advance the **LiteInst backend** so a real Detcore Tool drives guests via
LiteInst probe-based instrumentation, and more of the corpus runs under it.
Ratchet coverage upward with evidence; keep callback isolation correct.

## What this agent owns

- The LiteInst Guest/Tool integration in `reverie` (and `liteinst2/` tooling in
  the parent when an experiment needs it).
- LiteInst-specific handling in `hermit`.
- LiteInst example-tool / real-program exercises. For a tight per-backend
  iteration loop run **`make validate-liteinst`**, which runs ONLY the LiteInst
  strict corpus (wraps `validate.sh --liteinst-compat-only`); the full
  multi-backend suite is `./validate.sh`.

## Constraints

- **Additive Reverie API only** (see `AGENTS.md` Reverie API Policy); discuss
  core abstraction changes with the user first.
- LiteInst random-instrument (`LD_PRELOAD`) works on any **dynamic** ELF;
  fully static binaries (e.g. Go) are out of scope for the preload path — state
  this limitation rather than reporting a false failure.
- **Bind claims to Hermit+Reverie SHAs, backend, mode, and `L0/L1/L2`** with the
  exact programs named.
- Preserve callback isolation; do not let instrumentation state leak across
  fork/exec.

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

Own the named slot **`worktrees/liteinst/`** (nested layout v2:
`worktrees/liteinst/{hermit,reverie}`), one slot per agent. Provision it with
`scripts/allocate-worktree.rs --agent hermit-liteinst`. Preserve any dirty
LiteInst handoff in a `HANDOFF.md` before parking. Never feature-build in a
primary checkout. See `ai_docs/transient/worktree-management-map.md` for the
full protocol.

## Related

- [post-facto-review](post-facto-review/SKILL.md),
  [backend-reality-reviewer](backend-reality-reviewer/SKILL.md),
  [hermit-debugging](hermit-debugging/SKILL.md),
  [progress-rubric](progress-rubric/SKILL.md),
  [repo-cleanliness](repo-cleanliness.md).
