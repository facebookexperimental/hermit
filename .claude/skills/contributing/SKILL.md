---
name: contributing
description: "How a Hermit change is committed, reviewed and recorded: repositories and branches, commit and pull-request bodies, required PR sections, pull-request ownership through landing, implemented versus landed, task closure, PR-queue sweeps, the GitHub Actions trigger policy, unsatisfiable gates, and filing issues. Use before committing, opening, updating or landing a Hermit change, sweeping open pull requests, or filing an issue."
---

# Contributing to Hermit

Public contributors start with `CONTRIBUTING.md`; this skill is for agents and
maintainers. Update documentation for user-visible changes, and never publish
security vulnerabilities as ordinary issues.

## Repositories and branches

Hermit's primary copy lives in Meta's internal monorepo and is exported to
`facebookexperimental/hermit`. GitHub work uses feature branches and pull
requests on `facebookexperimental/hermit`, one coherent change per branch.
`rrnewton/hermit` `main` is a pure mirror: never push to it or open pull
requests against it. Main is gated: changes reach it only through the
project's validated landing path. Keep main green and repair a regression
before landing more work.

## Commit and pull-request bodies

- When the `dev-hermit` parent is available, get the disclosure with
  `./ci-hub/bin/who-am-i --tag --role ROLE` and paste it as the first line of
  the commit body and of the PR description. Never put it in a subject or
  title; keep titles concise, descriptive prose.
- Paste the exact tag, verbatim; never reconstruct or edit it. Short host
  names are fine; fully qualified host names never (owner, 2026-10-09).
- The first section after the disclosure is exactly **Plain Language Summary
  and Project Impact**: what capability, correctness property, evidence quality
  or developer workflow moves forward, its connection to the product vision or
  owner request, and the before/after difference. Administrative history,
  task bookkeeping and review mechanics come later. Record `Task: <task-id>`
  after it when the change implements task work.
- Every PR description also has **Determinism** (why the change is
  deterministic, with a logic or informal proof, not only tests), **Linux
  Semantics** and **Validation**. KVM changes add **Relationship to gVisor**. A
  `post-facto-human-review` PR adds **Human Review Required**, naming the
  numbered trigger, and its commit carries the `Post-facto-human-review:`
  trailer (`post-facto-review` skill).
- **Linux Semantics citations** (owner, 2026-10-18): a change to the Linux
  behavior Hermit or Detcore presents (a syscall's result, errno, restart or
  interruption; signal delivery, masks or handler order; `/proc` or `/sys`
  contents) cites the kernel function and `file:line` it matches at a named
  kernel tag, with a link, for example
  `https://git.kernel.org/pub/scm/linux/kernel/git/stable/linux.git/tree/kernel/signal.c?h=v6.9#n2850`,
  next to the native measurement the tests compare against. A measurement shows
  what one kernel did; the citation shows why. Reviewers check the citation.
- **Validation** reports the gates run and the assurance level reached, with
  backend, log level and relaxations (`validation-evidence` skill).

## Ownership through landing

The PR author owns the branch through landing: resolve review findings, rebase
when needed, rerun validation at the new exact head, and verify the landed
commit on freshly fetched `main`. The owner of an open PR is the single
`Owner: <agent-name> (<harness>, <host>) since <UTC timestamp>` line in its
description. A review attestation binds an exact SHA; a head move leaves it
bound to the older one.

The Reverie pin rule is stated once, in the OWNER-APPROVED RULE block at the top
of `scripts/check-reverie-pin.rs`. Read all three clauses: ancestry alone is not
the rule, because the pin may only advance.

## Records: implemented versus landed

Publishing a change and landing it are different facts; work that reads as
delivered while it never reached `main` is a recurring, expensive failure.

- **Product defects** (determinism bugs, wrong syscall behavior, crashes,
  backend gaps) go to a GitHub issue in `facebookexperimental/hermit`; search
  for an existing one first, including the older `rrnewton/hermit` tracker.
  **Durable findings** go in a GitHub issue, a PR comment or a tracked report;
  a to-do entry is not a record.
- **The record for a change is its PR**: the exact tested 40-hex head SHA, the
  base SHA, and the validation (exact commands and results, level, backend). A
  branch name alone is not evidence.
- TaskGraph (`tg`) is ORC's short-term to-do list; it is not a record and does
  not decide who owns a PR.

Rules (mandatory for every implementation and review agent):

1. **Implemented** means the change is pushed and under review, in a PR against
   `facebookexperimental/hermit` `main` or in the primary repository's code
   review, with that evidence. It claims no landing.
2. **Adversarial review confirms the work exists in the PR**: the claimed
   change is in the diff, the cited tests exist and ran at the PR head, and the
   reported level, backend and relaxations match reality. A claim that fails
   this is withdrawn in a PR comment.
3. **Unlanded work stays visible as unlanded.** Never describe an open,
   in-review, validation-red, awaiting-merge or dependency-blocked PR as
   landed. If the artifact disappears or the claim proves false, say so in the
   PR.
4. **Landed requires freshly fetched ancestry.** The change has landed when
   its commit is on the primary line by freshly fetched ancestry. A local green
   run, a GitHub state field or a label is not landing evidence; record the
   landed SHA in a PR comment.

When in doubt, claim the weaker state and say why in the pull request.

Examples:

- **Landed**: the change is on the primary line and freshly fetched ancestry
  confirms it; or, for a coordinated Hermit/Reverie change, both PRs
  merged, the parent gitlinks updated to the exact landed SHAs, and the pair
  revalidated.
- **Implemented, not landed**: PR open with exact-head validation green and
  awaiting merge; PR open but validation red or an exact-head receipt missing
  or stale (report the failure in the PR); work pushed but blocked on another
  PR or a Reverie pin bump (name the blocker and dependency SHAs).
- **Not done**: code uncommitted or unpushed (a stash is not a handoff); "it
  builds/tests pass locally" with no pushed branch or PR; a green local
  `cargo test` presented as completion (one backend passing is not done across
  backends); tests marked `#[ignore]`, masked or deleted to make a checkout look
  green.

### Under ORC: closing a TaskGraph task

Two tags carry the two facts. They are tags, not TaskGraph statuses: do not
invent a status that TaskGraph does not have. **You close your own task**:
when the PR is open with its evidence, add the `implemented` tag and close the
task yourself. Closure is not routed through a coordinator. Leaving a finished,
evidenced task open is itself a defect: it hides the task from every ORC queue
that reads status, and the parent's health tick reports `implemented` without
`closed` as a lifecycle violation. Preserve the task's existing tags:

```bash
tg note <task> "IMPLEMENTED: https://github.com/facebookexperimental/hermit/pull/<n> \
  | branch <feature-branch> @ <40-hex SHA> | base origin/main <SHA> \
  | validation: <exact commands + results, assurance level, backend>"
tg update <task> --tags <existing-tags>,implemented
tg update <task> --status closed
```

Whoever lands the change adds `landed` once rule 4 holds; the parent's
`./ci-hub/bin/close-task <task> --code <PR-or-full-SHA> --repo <owner/repo>
--source <checkout>` also writes the `CLOSURE-VERIFIED` note the health tick
reads. A claim withdrawn under rule 2 or 3 loses `implemented`.

## Sweeping the open-PR queue

Before deciding what is already landed, read `docs/PR_SWEEP_VERDICTS.md`.
Merge commits are disabled, so every landing rewrites the SHA and a comparison
by identity, ancestry or tree reports landed work as absent. A wrong "already
landed" closes work that is not on main, the only outcome in that drain that
loses something.

## A gate nobody can pass is a defect

Before adding or restating a landing requirement, check that it can be
satisfied from what is written down. Refuse both shapes of the same defect:
requiring evidence that cannot be produced (an exact-head local receipt while
the validation graph cannot complete, a GitHub result where Actions are
disabled), and requiring a named participant to act before anyone can proceed.
Any reader must be able to complete the protocol from the written text alone.
Fix such a requirement, rather than quietly routing around it, in every place
that states it at once; a copy left behind brings the old rule back.

## GitHub Actions

GitHub Actions results are supplemental evidence and gate nothing; no workflow
is a required status check. By owner decision (2026-10-27: "invoke CI
automatically only for external PRs not by our hermit-bot account") the small
`ci.yml` check runs on `pull_request` (opened, synchronize, reopened,
ready_for_review) unless the author is `hermit-bot3` or the PR is a draft, and
after a push to `integration` or `main` and on manual dispatch (`main` since
2026-11-12, for automated CI on the `facebookexperimental/hermit` export).
Since 2026-11-16 it is a small build-and-unit-test check (owner: "I'd rather it
check LESS but be reliable"); its header lists what it runs and where
everything else runs. The constructed portable DAG, `ci-portable.yml`, now runs
only on manual dispatch and after a push to `integration`. `ci.yml` uses no
secrets and a `contents: read` token, so it behaves the same for a branch and
for a fork. The privileged (self-hosted, PMU) workflows stay dispatch-only.
`scripts/check-github-actions-triggers.rs` enforces this shape: the bot/draft
gate on every job that can start on its own, and GitHub-hosted runners only for
every `ci.yml` job.

The checked-in triggers do not protect the self-hosted hosts: for a
`pull_request` event GitHub runs the workflow files from the PR itself, so a
fork can add or edit a workflow that targets the self-hosted labels. Repository
configuration keeps outside code off those hosts: Settings, Actions, "Fork pull
request workflows from outside collaborators" set to require approval for all
outside collaborators (not only first-time ones), and runner access that does
not admit pull requests from forks. A maintainer approving a fork run must
first read the pull request's `.github/` changes. What holds this line today is
agents choosing to hold it.

## Issues

GitHub Issues are the public tracker (`gh issue list -R
facebookexperimental/hermit`, `gh issue view <number> -R
facebookexperimental/hermit`; in Meta environments use the appropriate web
proxy). Network configuration is an environment requirement,
not an authentication workaround. Create, edit or close issues only when the
task calls for it; filing a product defect, as the records rules above require,
always does.
