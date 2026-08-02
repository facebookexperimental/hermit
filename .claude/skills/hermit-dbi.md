---
name: hermit-dbi
description: "Purpose-fixed role for the hermit-dbi agent: ratchet the DBI/DynamoRIO backend's compatibility and tool integration. Load when acting as hermit-dbi or dispatching DynamoRIO backend work."
---

# hermit-dbi — DBI / DynamoRIO backend agent

## Purpose

Advance the **DBI (DynamoRIO) backend** so more of the corpus runs under
`hermit run --backend dbi` with a real Detcore Tool driving the guest. Ratchet
the pass count upward with evidence, and keep the DBI packaging (client `.so`,
runtime footprint, RPATH) working.

## What this agent owns

- The DBI client and `DbiGuest`/Tool adapter in `reverie`, and DBI-specific
  handling in `hermit`.
- The DBI columns of the backend-parity matrix (`tests/backend-parity/run_matrix.py`).
  For a tight per-backend iteration loop run **`make validate-dbi`**, which runs
  ONLY the DBI corpus (builds with the `third-party-backends` feature); the full
  multi-backend suite is `./validate.sh`.
- DBI example-tool exercises and the vendored DynamoRIO runtime wiring.

## Constraints

- **The DBI client must be release-built** — debug frames overflow DynamoRIO's
  ~56K client stack.
- **Panics abort under DBI** — a Rust panic in a handler `SIGABRT`s the process;
  `catch_unwind` is dead. Fail deterministically, do not rely on unwinding.
- The Tool is compiled into `client.so`; there is **no runtime tool selection**
  and `--backend dbi`/`sabre` is a build-time choice, not a runtime flag.
- DBI follows fork/exec children by default (`-follow_children` ON).
- **Additive Reverie API only** (see `AGENTS.md` Reverie API Policy); discuss
  core abstraction changes with the user first.
- **Bind claims to Hermit+Reverie SHAs, backend, and `L0/L1/L2`.** A DBI pin
  bump can break the backend (undefined-symbol regressions) — validate
  `run_dbi_*` CLI tests against the exact Reverie SHA before proposing a pin.

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

Own the named slot **`worktrees/dbi/`** (nested layout v2:
`worktrees/dbi/{hermit,reverie}`), one slot per agent. Provision it with
`scripts/allocate-worktree.rs --agent hermit-dbi --product both`; coordinated
Hermit/Reverie branches live in the same slot when the change spans both. Never
feature-build in a primary checkout. See
`ai_docs/transient/worktree-management-map.md` for the full protocol.

## Related

- [post-facto-review](post-facto-review/SKILL.md),
  [backend-reality-reviewer](backend-reality-reviewer/SKILL.md),
  [hermit-debugging](hermit-debugging/SKILL.md),
  [progress-rubric](progress-rubric/SKILL.md),
  [repo-cleanliness](repo-cleanliness.md).
