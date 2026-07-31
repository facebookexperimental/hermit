---
name: post-facto-review
description: "Land reviewed, CI-green Hermit changes before human review and mark them for follow-up. Use as the default autonomous landing discipline."
---

# Post-Facto-Review Mode

## PR Comment Convention

Every PR description and comment created under this workflow MUST start with
the applicable role tag:

- `[impl agent, MODEL]` for implementation agents
- `[adversarial-reviewer agent, MODEL]` for review agents
- `[coordinator, MODEL]` for coordinator agents
- `[Human]` for the human owner

Examples: `[impl agent, gpt-5.6-sol]`,
`[adversarial-reviewer agent, opus-4.8]`.

The **currently-active** landing discipline for autonomous multi-agent work.
Changes land as soon as they are reviewed and CI-green; the human reviews them
*after* they are on `main` and fixes forward. This is the fast counterpart to
[human-review-first](../human-review-first/SKILL.md), which is dormant and gated on an
explicit human request.

> **Status: ON (default).** This is how the repo runs today. To switch to the
> cautious gate, the user must explicitly ask for
> [human-review-first](../human-review-first/SKILL.md) mode.

## The trade being made

Optimize for merge velocity while keeping a real quality bar. Autonomy is not an
excuse to skip review — key changes are still adversarially reviewed. The
difference from human-review-first is *ordering*: the human's review happens
after landing, and mistakes are corrected by follow-up commits rather than by
blocking the queue.

## 1. Exact human-review trigger set

Apply `post-facto-human-review` if and only if the PR contains at least one of
these four triggers:

1. **New syscall support.** Verify `AUTONOMOUS-BOT-IMPLEMENTED` at the new
   dispatch/classification entry and `TODO-HUMAN-REVIEW(PR-id)` at the
   implementation or determinization block.
2. **A Reverie API or core-abstraction change**, including the `Tool`, `Guest`,
   `Backend`, or syscall-interception model.
3. **A new determinization strategy**, rather than an implementation of an
   already established strategy.
4. **A core DetCore scheduling change**: anything that affects how programs are
   scheduled, especially race-search behavior. This trigger is always labeled.
   [Hermit PR #1151](https://github.com/rrnewton/hermit/pull/1151), which moved
   slowdown into virtual-time/epoch scheduling, is the canonical good example
   of both this trigger and the determinism rationale reviewers need.

Routine backend-parity work toward the golden ptrace reference does **not**
trigger human review merely because it changes a non-ptrace backend. Label it
only when it also meets one of the four triggers above; "backend parity change"
is not a valid rationale by itself.

Before landing a triggered change, use independent reviewers whose job is to
refute the change over repeated author-fix/reviewer-recheck rounds. Cover
correctness, determinism, the Reverie/Detcore boundary, and security, and bind
evidence to exact commands and SHAs.

## 2. Mandatory PR description sections

Every PR description must contain:

- **Summary**.
- **Determinism** — mandatory for every PR; explain why the change is
  deterministic and give the logic or informal proof, not only test results.
  For any time/clock/scheduling change, the proof must argue that virtual time
  stays **continuous and fine-grained** (see
  [continuous-virtual-time-is-sacred](../continuous-virtual-time-is-sacred/SKILL.md));
  a change that achieves determinism or parity by rounding, freezing,
  coarsening, or resetting time is a red flag, not a proof.
- **Linux Semantics** — state how the change matches real Linux kernel behavior
  for the affected syscalls/interfaces (return values, error codes, ordering,
  edge cases), or explain why a deliberate deviation is safe. Determinism must
  not come at the cost of guest-visible semantics.
- **Validation** — exact commands, outcomes, limitations, and relaxations.
  Parity/determinism evidence must demonstrate **continuous evolution** —
  repeated reads across the run, and cross-exec/cross-thread/cross-backend
  samples where relevant — **not a single first-sample** match. First-read
  agreement on a tidy origin is a classic false green.
- **Relationship to gVisor** — required for KVM changes; state the comparison
  or explain why none applies.
- **Human Review Required** — mandatory when `post-facto-human-review` is
  applied. Name the specific numbered trigger(s); vague prose such as "backend
  change" is insufficient.

For a `post-facto-human-review` PR these sections are not merely convention:
the `core-review-protocol` merge-gate job runs
`scripts/core-review-protocol-lint.sh`, which **blocks landing** unless the PR
also carries dual adversarial-review rounds
(`adversarial-review-codex<N>` + `adversarial-review-claude<N>`, N in 1..4) and
current dual approval (`passed-review-codex` + `passed-review-claude`). A new
push drops the `passed-review-*` labels, so approval must be re-earned on the
latest revision.

PR #1151 is the canonical good example for trigger 4: its slowdown model is
explained as weighted virtual-time progression with deterministic epochs and
replay evidence, rather than asserted from passing tests alone. New PRs must
use the section names above and identify trigger 4 explicitly.

### Dual adversarial review for core determinism/time/scheduling changes

A change to core determinism, virtual time, or the scheduler (any of triggers 3
and 4, and any clock/time-virtualization change) must survive **two independent
adversarial reviews before landing — one by a `claude` agent and one by a
`codex` agent**. Using two different model families reduces correlated blind
spots on exactly the changes where a subtle determinism regression is most
costly and hardest to see. Each reviewer's job is to *refute* the change over
repeated author-fix/reviewer-recheck rounds, covering correctness, continuous
fine-grained virtual time, the Reverie/Detcore boundary, Linux semantics, and
security, with evidence bound to exact commands and SHAs. Do not land such a
change until both reviews are resolved (and authoritative CI is green). Routine,
single-backend parity work that meets none of the triggers keeps the standard
single-reviewer bar.

## 3. Labels and new-syscall code markers

- `post-facto-human-review` is the single routing label for a PR awaiting the
  human's after-the-fact review. Apply it only for the four triggers above.
- Apply `pre-land-human-review` only when the owner explicitly requests it;
  never infer or auto-apply it. Never alter `human-approved`.
- The obsolete `human-review` and `post-facto-review` labels must not be used.

New syscall support authored by a bot must carry both narrowly scoped audit
tags: `// AUTONOMOUS-BOT-IMPLEMENTED` at its new dispatch/classification entry
and `// TODO-HUMAN-REVIEW(PR-id)` at its implementation or determinization
block. These are not blanket markers for API changes, backend work, or routine
parity fixes.

## 4. Land immediately after review + CI green

Once a triggered change survives adversarial review and CI is green, **land it** —
squash-merge to `main`. Do not wait for a human.

- Merge gate = **GitHub-managed portable "Regular tests" green**. The privileged
  "Host-dependent tests" lane is environmental and non-required (`main` is
  unprotected); a red privileged lane does not block landing.
- Prefer merging on real GitHub-managed portable green. When using `--admin`, it should
  only be bypassing the known-environmental privileged lane, not a genuine
  red on GitHub-managed portable or on a meaningful check.
- When one of the four triggers applies, add `post-facto-human-review` and
  verify that **Human Review Required** names the specific trigger before
  landing. Do not label routine backend-parity work.
- After landing, rebase dependent PRs onto the new `main` (see the PR DAG
  section of [human-review-first](../human-review-first/SKILL.md)).

## 5. Human reviews post-facto, fix-forward

The human reviews landed changes after the fact (aided by the labels and code
markers above). Corrections are made by **follow-up commits/PRs**, not by
reverting the queue — fix forward. If a human review finds a real defect, open a
fix PR that removes the relevant `// TODO-HUMAN-REVIEW` marker once addressed.

## CONFIRM BEFORE CLOSING

For a **KEY API / core-abstraction change** to the Reverie `Tool`, `Guest`,
`Backend`, or interception model, loudly report the change and its implications
to the owner. Do not close the corresponding task until the owner has discussed
the API change.

This is a task-closure gate, not an automatic pre-land gate: the post-facto
landing default still applies after adversarial review and authoritative CI are
green. Apply `pre-land-human-review` only on the owner's explicit request;
never infer or auto-apply it from the nature of the change.

## Deactivation

Switch to [human-review-first](../human-review-first/SKILL.md) when the user explicitly
asks for it. Announce the switch; from that point every key change waits for
human approval *before* landing.
