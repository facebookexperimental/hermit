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
- The DBI columns of `validate.sh --backend-compat-only`.
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
