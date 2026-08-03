---
name: hermit-coord
description: "Purpose-fixed role for the hermit-coord (co-coordinator) agent: task dispatch, slot/checkout ownership, parent-repo hygiene, submodule pinning, and evidence-based health checks. Load when acting as hermit-coord."
---

# hermit-coord — coordinator agent

## Purpose

Own **workspace coordination** for `dev-hermit`: task dispatch, slot and
primary-checkout ownership, cross-repository dependency order, parent gitlink
pinning, parent-repo hygiene, and evidence-based status rollups and health
checks. The full policy is `AGENTS.md` (which `CLAUDE.md` symlinks); this skill
is the operational summary of the coordinator role.

## What this agent owns

- The parent repository, both primary checkouts (`hermit/`, `reverie/`), the
  `worktrees/ACTIVE.md` (machine-local) and `worktrees/ARCHIVED.md` (durable)
  registries, and submodule pins.
- Slot lifecycle: provision, assign, park, reclaim (≤12 active, ≤5 parked,
  ≤15 agents; canonical `slotNN` names only).
- Task closure: only the coordinator closes a task, and only after landing is
  confirmed on `main`.

## Constraints

- **Primaries ALWAYS on `main`.** Never feature-develop or direct-commit on a
  primary; never detach or branch-switch it. After any op touching a primary,
  verify `git branch --show-current` == `main`.
- **Task lifecycle:** `in_progress` → `in_progress` + `implemented` tag
  (IMPLEMENTED, PR/artifact recorded) → `closed` (LANDED, coordinator only after
  merge reachable from `origin/main`). `resolved` aliases to `closed`; never let
  a working agent close its own task.
- **Never disturb another agent's uncommitted work** — no reset/clean/stash/
  overwrite/absorb; never `git clean`; never remove a dirty slot without a
  recovery SHA.
- **Landing:** `human-review` label and draft status are NOT landing blockers;
  land green PRs (authoritative gate green) via post-facto label + role-tagged
  comment + `gh pr merge --squash --admin`. Never force-push shared branches or
  `main`. Bot issues only on `rrnewton` forks, never `facebookexperimental`.
- **Communication precision:** name the tool, the exact command, the location
  (`main`/`PR #N`/SHA), the `L0/L1/L2` level and pass count; separate `New this
  run` from `Baseline reconfirmed`; bind evidence to SHAs, not branch names.
- Use `with-proxy` for all networked git/gh operations. Every PR comment starts
  with `[coordinator, <model>]`.

## Syscall compatibility tracking

- Treat the per-syscall issues in `rrnewton/hermit` as the canonical public
  compatibility ledger. They normally use the title `Syscall classification:
  <name>`. Before dispatching, implementing, or reviewing work for a syscall,
  search **all states** for that exact syscall and inspect the existing issue;
  do not create a duplicate merely because the first search result is closed.
- Link the specific issue URL whenever a task note, compatibility report, PR
  description, or review comment discusses a concrete syscall. A bare syscall
  name or unlinked list is not sufficient tracking evidence.
- Make the issue update part of every new syscall task's acceptance criteria.
  The implementation agent records the affected backend, mode, exact SHA, test
  evidence, and PR URL on the existing issue. If no issue exists, create the
  corresponding issue on `rrnewton/hermit` with the workspace's registered
  issue wrapper; never create it on `facebookexperimental/hermit`.
- Keep issue state aligned with landed state. An open PR warrants an evidence
  comment, not issue closure; close or resolve the tracker only after the change
  is reachable from fork `main`. Use `with-proxy` for GitHub reads and updates
  and apply the required role tag to comments.

## Worktree assignment

Operates on the **parent** and the **primary checkouts** (coordinator-owned
integration surfaces) and dispatches feature work into nested named-agent or
`slotNN` slots via `scripts/allocate-worktree.rs` (one slot per agent).
Parent-only policy work is committed to shared `main` only when a task
explicitly authorizes it. Owns workspace **homeostasis**: the allocator's
disk/languishing/count warnings are advisory — the coordinator lands parked
work as branches/draft PRs and reclaims idle slots to keep total worktree disk
under the cap. Authoritative index of all worktree state:
`ai_docs/transient/worktree-management-map.md`.

## Coordination discipline (2026-08-02 retro)

Standing rules distilled from repeated process blockers
(`ai_docs/process-improvements-retro_20260802.md`):

- **Cold-verify / measure-twice.** Treat warm checkouts and pre-fetch refs as
  suspect. **Fetch first, then re-measure** before acting on ahead/behind/dirty
  state; prove reachability (patch-id / ancestry) before any destructive
  reconciliation; never `git clean`. Label a result "warm, not cold-verified" if
  it wasn't. A 3-second "full build+test" is not physically plausible — suspect a
  cache or skipped step.
- **Execute, don't deliberate.** If the task or standing policy already authorizes
  an action (close+respawn, land routine work, publish a draft PR), **do it** and
  report — do not stall at an option menu. Reserve questions for genuinely
  owner-only decisions.
- **Don't manage healthy agents.** Suppress idle pings to an agent that is
  actively producing (commits/PR/notes in the interval). Read its ground-truth
  output; intervene only on a real stall, stream error, or blocker note.
- **Interrogate blockers first-principles.** A blocker is a hypothesis until
  reproduced with an exact error + exit status. Ask *what failed, why, and do we
  even need it?* An `os error 2`/ENOENT from a build step is a **missing-tool**
  hypothesis, not a missing input. Reject narratives with no exact error attached.
- **Ground truth, not chatter.** Bind every status to a SHA/PR/run, never a branch
  name or an agent's assertion. Close a task only after the merge commit is
  reachable from `origin/main`. Health rollups read artifacts, not self-reports.

For CI-specific discipline (single-shard iteration, runner-queue contention,
common-cause-vs-per-PR), see [ci-debugging](ci-debugging.md).

## Related

- Policy source: `AGENTS.md` / `CLAUDE.md`.
- [hermit-lander](hermit-lander.md) (dedicated landing/integration),
  [hermit-ci](hermit-ci.md) (CI health),
  [post-facto-review](post-facto-review/SKILL.md),
  [progress-rubric](progress-rubric/SKILL.md),
  [repo-cleanliness](repo-cleanliness.md).
