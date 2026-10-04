# Intent brief for validation and landing infrastructure

This brief is derived from the preserved owner prompts, not from the current
implementation.  It is the standard used to review the implementation and the
tutorial.

## Outcome

Hermit should stay green while reviewed changes move to `main` without a queue
of finished work.  Local validation is the current authority.  A contributor
should be able to discover the commands, run validation, hand the exact commit
to a reviewer, and understand whether it landed and reached the checkout that
operators use.  The owner measures success by a green `main`, regular movement
of commits to `main`, a clear compatibility scorecard, and PRs that do not
languish ([turns 752–753](2026-08-11-to-12.md#orc-turns-752753--2026-08-11-2131-utc)).

## Required workflow

1. The implementer works in one explicitly assigned directory tree.
2. The implementer runs the selected local validation on the exact commit and
   records a qualifying result through `ci-hub`.
3. The implementer hands the exact commit to the required adversarial reviewer.
4. The reviewer either returns concrete changes or lands the commit.  Review
   cannot become a holding queue.
5. A soft-green land is followed immediately by validation of the new `main`.
   If `main` is red, other landing stops and the regression is fixed forward.
6. Landing is followed by a deployment check: the primary checkout or running
   tool must actually use the landed commit.

This is the owner's implementer-validates, reviewer-lands-or-bounces protocol
([turn 1092](2026-08-11-to-12.md#orc-turn-1092--2026-08-12-062421-utc)).
Critical changes retain one Codex and one Claude review and the post-facto human
review label ([turns 875, 877, and 878](2026-08-11-to-12.md#orc-turns-875-877-and-878--2026-08-11-23572358-utc)).

## Design constraints

- There is one source of truth for each decision and other documentation links
  to it instead of copying it ([turns 504 and 512](2026-08-11-to-12.md#one-source-of-truth-and-local-validation)).
- The active ledger has one strongly typed record shape.  A bulk reader and a
  direct `jq` query see the same fields, including git depth
  ([turns 1018–1027](2026-08-11-to-12.md#orc-turns-1018-1020-1023-1026-and-1027--2026-08-12-05310541-utc)).
- Rust code and Rust scripts reject warnings.  Existing Python is strictly
  typed and checked without warnings ([turns 898 and 1028](2026-08-11-to-12.md#orc-turns-898-and-1028--2026-08-12-0008-and-0542-utc)).
- Commands with subcommands provide idiomatic global and per-subcommand help
  ([turns 914–918](2026-08-11-to-12.md#orc-turns-914915-and-918--2026-08-12-00170019-utc)).
- A lock or worktree owner can die.  Recovery is explicit and observable; a
  compiled but unreachable release path is not a feature
  ([turn 513](2026-08-11-to-12.md#orc-turn-513--2026-08-11-174333-utc)).
- Live coordinator infrastructure changes out of place, as one complete change,
  so users do not execute a half-edited tool
  ([turn 568](2026-08-11-to-12.md#orc-turn-568--2026-08-11-181946-utc)).
- Ownership is exclusive for a directory tree, not merely for individual files
  ([turn 576](2026-08-11-to-12.md#orc-turn-576--2026-08-11-182501-utc)).
- Agents do not invent terminology.  Documentation uses project and owner
  language or explains the fact plainly
  ([turn 1098](2026-08-11-to-12.md#orc-turn-1098--2026-08-12-062729-utc)).
- No test, threshold, comparator, or classification is weakened to create a
  pass.  Review explicitly tries to detect that failure
  ([turns 1063 and 1117](2026-08-11-to-12.md#no-goalpost-moving-and-no-invented-terminology)).

## Documentation test

A first-time contributor should be able to answer these questions from one
tutorial without knowing internal file names:

1. What command do I run while iterating?
2. What command produces landing evidence, and where can I inspect it?
3. What makes a hard-green or soft-green commit landable?
4. Who reviews, who lands, and what happens when review finds a defect?
5. What runs immediately after landing?
6. How do I prove the landed change reached the primary checkout or running tool?
7. How do I find and recover a stale validation or worktree owner safely?
8. Which `--help` or focused user guide describes each command in detail?

If the answers require contradictory documents, unexplained record variants,
an unlisted side file, or a mechanism that exists only as dead code, the
infrastructure fails the owner's documentation test
([turn 1121](2026-08-11-to-12.md#single-threaded-overhaul)).

## Evidence required for an improvement claim

Evidence names the exact commit, command, population, unit, and denominator.
For this overhaul, compare before and after on at least:

- number of open PRs whose purpose is validation or infrastructure;
- number of active documents that contradict the current local-only policy;
- number of public commands in the tutorial with a working discovery path;
- active-ledger record shapes accepted by the authoritative reader;
- Rust compiler warnings emitted by public infrastructure entrypoints; and
- success and refusal tests for any changed gate.

An exit code alone is not evidence that a tutorial or help page makes sense.
The user-visible output must also be read as a first-time user.
