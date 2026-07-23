---
name: human-review-first
description: "Gate key Hermit changes on adversarial review and explicit human approval. Use only when the user explicitly requests human-review-first mode."
---

# Human-Review-First Mode

A landing discipline for autonomous multi-agent work in which **no KEY
change reaches `main` until a human has approved it.** (Key changes
defined below.) This is the *cautious* counterpart to
[post-facto-review](../post-facto-review/SKILL.md), which is the
currently-active default.

> **Status: OFF by default.** This mode is dormant institutional knowledge.
> Only activate it when a human explicitly asks for it (see below). While it is
> off, the repository runs under [post-facto-review](../post-facto-review/SKILL.md).

## When to activate

Activate **only** when the user explicitly says something equivalent to
**"human review first mode"** (e.g. "turn on human-review-first", "gate landings
on my review", "nothing lands without me"). Do not infer it from caution, risk,
or the nature of a change. It is a deliberate, human-thrown switch.

On activation, announce the switch, and from that point apply the protocol below
to every PR that touches a **key change** (defined next).

## What counts as a "key change"

Key changes require the full human-review-first protocol. Non-key changes may
still land under the lighter path, but when in doubt treat a change as key.

**Key (gate on human approval):**
- **New syscalls** — any new syscall handler, or a change to which syscalls are
  intercepted/emulated/forwarded (`detcore/src/syscalls/**`).
- **Major Reverie API changes** — new traits/trait methods, changes to the
  `Tool`/`Guest`/`GlobalTool` surface, event-dispatch semantics, or the
  reverie↔detcore boundary. Cross-repo (`rrnewton/reverie` ↔ `rrnewton/hermit`)
  changes are key by default.
- Scheduler/determinism-model changes, record/replay format changes, anything
  that alters guest-visible behavior or the determinism guarantee.

**Not key (small extensions are OK without the full gate):**
- Small, additive extensions to an existing Reverie API that preserve existing
  behavior (e.g. one new optional method with a default, a new enum variant
  behind a match arm).
- Bugfixes
- Tests, docs, comments, benchmarks, CI wiring, refactors.

## Protocol: adversarial review → human approval → THEN land

The ordering is strict. A key change lands **only** after all three complete, in
order:

1. **Adversarial review.** Spawn independent reviewer agents whose job is to
   *refute* the change, not bless it. Multiple rounds; each round the author
   fixes and the reviewers re-attack. Cover correctness, determinism regressions
   (does it preserve L1/L2/L3 per AGENTS.md?), the reverie/detcore boundary, and
   security. Record what was run and observed (no vague "looks good").
2. **Human approval.** Present the diff, the adversarial-review findings, and
   local test evidence to the human. Landing waits for an explicit human
   approval. Apply the `human-approved` label **only when the human has actually
   approved** — never self-apply it (see [post-facto-review](../post-facto-review/SKILL.md)).
3. **Land.** Only after 1 and 2, land the PR (squash), then rebase dependents
   (see PR DAG below).

Never reorder these. Under human-review-first, "CI is green" is necessary but
**not sufficient** — human approval is the gate.

## Frontier branch: speculative integration

`frontier` is the speculative integration branch that merges the in-flight PRs
in dependency order so agents can build on not-yet-landed work without waiting
for human approval of each piece.

- Feature branches target `main` for landing, but may be **stacked** and
  integrated on `frontier` first for end-to-end testing.
- `frontier` is rebuilt as PRs land on `main`: after a `main` advance, rebase
  `frontier` (and the still-open stacked PRs) onto the new `main`.
- `frontier` itself is **never** merged to `main` (the integration PR carries a
  "do not merge" note). Content reaches `main` only through the per-PR
  human-approval gate above.
- Keep `frontier` green enough to be useful; when it diverges badly, rebuild it
  from `main` + the current open stack rather than accreting merge commits.

## PR DAG management

Open PRs form a dependency DAG via their base branches (a PR based on another
feature branch depends on it). Managing it:

- **Merge order matters.** Land bottom-up: a PR whose base is `main` before any
  PR stacked on top of it. Landing a base PR (squash) rewrites history, so
  retarget + rebase its children onto `main` afterward.
- **Rebase on main advances.** Every time `main` moves, DIRTY (conflicting)
  children must be rebased by their owners onto the new `main`. The owner holds
  the context; do not force-push someone else's active branch.
- **Identify review priority** by downstream unblock count: review first the key
  PR that the most other PRs are stacked on.
- Build (or refresh) a graphviz DOT of `base → head` edges when the stack is
  large enough to be hard to reason about.

## Deactivation

Switch back to [post-facto-review](../post-facto-review/SKILL.md) when the user says so
(e.g. "back to post-facto", "autonomous landing on"). Announce the switch. From
that point, key changes still get adversarial review but land immediately after
review + CI green, with post-facto human review and fix-forward.
