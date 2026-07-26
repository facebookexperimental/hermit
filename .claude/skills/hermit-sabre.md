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
- The SaBRe columns of `validate.sh --backend-compat-only`.

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
