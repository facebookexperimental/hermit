---
name: post-facto-review
description: "Land reviewed, CI-green Hermit changes before human review and mark them for follow-up. Use as the default autonomous landing discipline."
---

# Post-Facto-Review Mode

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

## 1. Key changes still get adversarial review (multiple rounds)

The "key change" definition is identical to
[human-review-first](../human-review-first/SKILL.md): new syscalls, major Reverie API
changes (small additive extensions are OK), scheduler/determinism-model changes,
record/replay format changes.

Before landing a key change:
- Spawn independent reviewer agents whose job is to **refute** the change, over
  **multiple rounds** — author fixes, reviewers re-attack — until it survives.
- Cover correctness, determinism (preserve L1/L2/L3 per AGENTS.md), the
  reverie/detcore boundary, and security.
- Ground every claim in evidence (exact command + observed output), per
  AGENTS.md "Precise Communication". No vague "works"/"looks good".

## 2. Labels

- `human-review` — marks a PR the human still wants to look at. **Never
  auto-close or auto-land a `human-review` PR.** Under post-facto mode these
  stay open for the human even though other work lands around them.
- `post-facto-review` — marks a PR that landed autonomously and is awaiting the
  human's after-the-fact review.
- **Never apply `human-approved`.** That label means a human actually approved,
  and only a human may apply it. A bot claiming approval is a defect.
- `locally-validated` — the legitimate substitute for green CI when the CI lane
  cannot go green for environmental reasons: run the checks the PR can affect
  locally, prove any residual failure is baseline/environmental, then label +
  merge on real GitHub-hosted green where possible (avoid `--admin` over red CI).

## 3. Code markers

Autonomously-landed code carries in-source breadcrumbs so a human reviewing
post-facto can find exactly what a bot wrote and what still needs eyes:

- `// AUTONOMOUS-BOT-IMPLEMENTED` — this code was written and landed by a bot
  without prior human review.
- `// TODO-HUMAN-REVIEW(PR-id)` — a specific spot the human should scrutinize;
  include the PR number, e.g. `// TODO-HUMAN-REVIEW(#206)`.

Keep markers at the smallest meaningful scope (the function/block that is
novel), not blanketed across untouched code.

## 4. Land immediately after review + CI green

Once a key change survives adversarial review and CI is green, **land it** —
squash-merge to `main`. Do not wait for a human.

- Merge gate = **GitHub-hosted "Regular tests" green**. The self-hosted
  "Host-dependent tests" lane is environmental and non-required (`main` is
  unprotected); a red self-hosted lane does not block landing.
- Prefer merging on real GitHub-hosted green. When using `--admin`, it should
  only be bypassing the known-environmental self-hosted lane, not a genuine
  red on GitHub-hosted or on a meaningful check.
- After landing, rebase dependent PRs onto the new `main` (see the PR DAG
  section of [human-review-first](../human-review-first/SKILL.md)).

## 5. Human reviews post-facto, fix-forward

The human reviews landed changes after the fact (aided by the labels and code
markers above). Corrections are made by **follow-up commits/PRs**, not by
reverting the queue — fix forward. If a human review finds a real defect, open a
fix PR that removes the relevant `// TODO-HUMAN-REVIEW` marker once addressed.

## Deactivation

Switch to [human-review-first](../human-review-first/SKILL.md) when the user explicitly
asks for it. Announce the switch; from that point every key change waits for
human approval *before* landing.
