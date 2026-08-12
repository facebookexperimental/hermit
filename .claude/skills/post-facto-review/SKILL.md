---
name: post-facto-review
description: "Current Hermit post-facto human-review protocol: exact trigger set, dual Claude+Codex adversarial review for triggered changes, the close precondition an adversarial reviewer must satisfy before closing a PR, exact-head validation, and fix-forward human review after landing."
---

# Post-facto human review

Every bot-authored PR description or comment starts with the applicable
`[impl agent, MODEL]`, `[adversarial-reviewer agent, MODEL]`, or
`[coordinator, MODEL]` tag. Human comments use `[Human]`.

Immediately after that disclosure, every PR description starts with `## Plain
Language Summary and Project Impact`. It states the substantive outcome, its
connection to the product vision or owner request, and the meaningful
before/after difference. Administrative history and review mechanics follow.

Apply `post-facto-human-review` if and only if the PR has one of these triggers:

1. New syscall support, with `AUTONOMOUS-BOT-IMPLEMENTED` at the new dispatch or
   classification entry and `TODO-HUMAN-REVIEW(PR-id)` at the implementation or
   determinization block.
2. A Reverie `Tool`, `Guest`, `Backend`, syscall-interception, or other core API
   abstraction change.
3. A new determinization strategy, not routine implementation of an established
   one.
4. A core DetCore scheduling change affecting how programs are scheduled,
   especially race search.

Routine parity work toward the ptrace reference is not a trigger by itself.
The label routes after-the-fact human review and never waits for human approval
before landing. Never apply `pre-land-human-review`, alter `human-approved`, or
recreate obsolete review labels under the current owner directive.

## Adversarial review and evidence

A triggered PR requires independent exact-head approval from one Claude-family
reviewer and one Codex-family reviewer. Neither is the author. Role-tagged review
comments carrying the full head SHA are authority; numbered review and
`passed-review-*` labels are caches. Any push invalidates both approvals.

Every PR contains `Plain Language Summary and Project Impact` first, then
`Determinism`, `Linux Semantics`, and `Validation`.
KVM changes also contain `Relationship to gVisor`; a triggered PR contains
`Human Review Required` naming the numbered triggers. A determinism proof
explains the model, not only tests.

For non-KVM L2 evidence, use `--verify --verify-strict --verify-json` and require
`bitwise_parity: true`. Exit status/stdout/stderr are byte-equal; INFO events use
the declared `BitwiseInfoV1` envelope (only the wall-clock prefix is removed and
host addresses are ordinalized, while virtual time, branch counts, syscall
values, sizes, flags, and other payloads remain exact). Default `--verify` is
lossy, and KVM is output/status-only, so neither is full L2 INFO parity.
First-sample agreement is not proof of a continuously evolving clock.

## Close precondition

Closing a pull request is the reviewer's last resort, not a review verdict. The
default outcome of an adversarial review that finds defects is to **update the
PR in place**: push a corrected head to the same branch, restate the objection
against the new head, and let the one PR carry the work forward. A PR generally
should be UPDATED, not replaced. Three predicates gate a close:

1. **No close without a named successor or an explicit owner instruction.** The
   closing comment contains a literal `SUPERSEDED-BY: #<n>` naming the open pull
   request that carries the same work forward, or it quotes an explicit owner
   instruction to close this PR. A closing comment carrying neither is invalid:
   reopen the PR and update it in place. "The head is recoverable" is not a
   successor — a recoverable SHA is a rescue path, not published work.
2. **A rejection names the concrete change required.** State the exact defect
   and the exact change that resolves it: the file, the gate, the missing
   evidence, or the command whose output would settle it. "Procedurally
   deficient", "not closure-grade", and "does not meet the bar" are verdicts,
   not reviews. A reviewer who cannot name what to change has not finished the
   review, and the PR is not ready to be rejected.
3. **"A task exists in the TaskGraph" is NOT a valid reason to close.**
   Deferring the work to a task does not satisfy rule 1 and never substitutes
   for a successor PR. A task is neither a guarantee the work happens nor an
   artifact on `main`. Owner-cited fleet measurement, 2026-08-07: 106 tasks
   tagged `implemented` → 38 landed → 4 met their stated goal. Closing a PR
   against a task trades a reviewable diff for that attrition.

Hermit [PR #1635](https://github.com/rrnewton/hermit/pull/1635) is the incident
these rules encode: a critical-path change was closed without landing, with no
successor PR named and remediation deferred to two TaskGraph tasks. It then had
to be reopened and updated in place — which is what rule 1 requires first.

## Landing

Inside dev-hermit, the parent `AGENTS.md` and ci-hub executable are canonical:
the exact current Hermit head needs a clean, counted, full-profile receipt
accepted by `ci-hub validate-status`. A `locally-validated` label, command exit,
or comment is only a cache. GitHub checks are supplemental; a genuine failure
they reveal still blocks. A standalone checkout follows its current
repository-defined exact-head authority.

Land only when the task authorizes it, required adversarial review is resolved,
and the semantic verifier accepts the current head. Use the serialized tracked
landing path, never `--admin`, then fetch main and prove ancestry. Human review
happens after landing and corrections fix forward.
