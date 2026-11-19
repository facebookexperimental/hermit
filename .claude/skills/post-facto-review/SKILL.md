---
name: post-facto-review
description: "Current Hermit post-facto human-review protocol: exact trigger set and commit trailer, review depth (two models for engine changes, otherwise one reviewer), the close precondition an adversarial reviewer must satisfy before closing a PR, the hand-run protocol lint, and fix-forward human review after landing."
---

# Post-facto human review

Every bot-authored PR description or comment starts with the applicable
`[impl agent, MODEL]`, `[adversarial-reviewer agent, MODEL]`, or
`[coordinator, MODEL]` tag. Human comments use `[Human]`.
The tag belongs in the description or comment body, never in the PR title.

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
   especially race search. Hermit
   [PR #1151](https://github.com/rrnewton/hermit/pull/1151), which moved
   slowdown into virtual-time/epoch scheduling, is the canonical good example.

Routine parity work toward the ptrace reference is not a trigger by itself.
Every triggered change also carries this trailer in its commit message, because
a pull-request label does not survive repository export while a trailer travels
with the commit on every copy of the history:

```text
Post-facto-human-review: trigger-<n> (<short reason>)
```

`<n>` is the trigger number above (repeat the line for several triggers) and
the reason is a few words. Find tagged changes with
`git log --grep='^Post-facto-human-review:'` in any clone, `git log -1
--format=%B <commit>` inside a bisect script, or
`gh pr list --label post-facto-human-review` for pull requests.

The label routes after-the-fact human review and never waits for human approval
before landing. Never apply `pre-land-human-review`, alter `human-approved`, or
recreate obsolete review labels under the current owner directive.

## Adversarial review and evidence

Review depth is the same everywhere: one cold-context reviewer who is not the
author by default, and two reviewers on two different models, neither the
author, for engine changes (core Reverie types, the Detcore engine and its
scheduler). A triggered PR gets exact-head approval at that depth. Role-tagged
review comments carrying the full head SHA are authority; numbered review and
`passed-review-*` labels are caches. Any push invalidates the approvals.
`scripts/core-review-protocol-lint.sh` encodes the same depth: one exact-head
seat in any lane (claude, codex or muse) by default, and two seats in two
different lanes when a `REVIEW-DISPATCH-ATTESTATION` in the comment history
names a critical class (`hermit-syscall`, `detcore-scheduling`,
`reverie-api`) — the classification carries across a push, so a new head does
not drop the change back to one seat; it requires no round or approval
labels.

Every PR contains `Plain Language Summary and Project Impact` first, then
`Determinism`, `Linux Semantics`, and `Validation`.
KVM changes also contain `Relationship to gVisor`; a triggered PR contains
`Human Review Required` naming the numbered triggers. A determinism proof
explains the model, not only tests. For a change to the Linux behaviour Hermit
presents (syscall results, errno, restart or interruption, signals, `/proc`),
`Linux Semantics` cites the kernel source it matches (function and `file:line`
at a named kernel tag, with a link) beside the native measurement; check that
the cited code says what the PR claims.

For L2 evidence, use `--verify --verify-strict --verify-json` and require
`bitwise_parity: true`. Exit status/stdout/stderr are byte-equal; INFO events use
the declared `BitwiseInfoV1` envelope (only the wall-clock prefix is removed and
host addresses are ordinalized, while virtual time, branch counts, syscall
values, sizes, flags, and other payloads remain exact). Default `--verify` is
lossy and cannot establish L2. A KVM cell is L2 only under the stricter report
rules in the `validation-evidence` skill. First-sample agreement is not proof of
a continuously evolving clock.

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
3. **"A task exists in the TaskGraph" (or any to-do list) is NOT a valid reason
   to close.** Deferring the work to a task does not satisfy rule 1 and never substitutes
   for a successor PR. A task is neither a guarantee the work happens nor an
   artifact on `main`. Owner-cited fleet measurement, 2026-08-07: 106 tasks
   tagged `implemented` → 38 landed → 4 met their stated goal. Closing a PR
   against a task trades a reviewable diff for that attrition.

Hermit [PR #1635](https://github.com/rrnewton/hermit/pull/1635) is the incident
these rules encode: a critical-path change was closed without landing, with no
successor PR named and remediation deferred to two TaskGraph tasks. It then had
to be reopened and updated in place — which is what rule 1 requires first.

## Run the protocol lint by hand

For a labeled PR, `scripts/core-review-protocol-lint.sh` checks the labels, the
body sections, exact-head review comments and outstanding refusals. **Nothing
runs it for you and nothing blocks a landing that skips it**: its only caller,
`.github/workflows/merge-gate.yml`, is `workflow_dispatch:` only, `main` has no
branch protection, and the main ruleset is active with no rules (measured
2026-08-26). An earlier text claimed a blocking merge-gate job; a false
enforcement claim is worse than a missing gate, because a reader who believes
landing is gated has no reason to run the check. The script itself is sound and
its self-test must pass. Run it before landing a labeled PR:

```bash
set -euo pipefail
repo=facebookexperimental/hermit
pr=<N>

if ! pr_json=$(with-proxy gh api "repos/$repo/pulls/$pr"); then
  echo "pull-request fetch FAILED -- this is not a pass" >&2
  exit 2
fi
labels=$(jq -r '.labels[].name' <<<"$pr_json")
body=$(jq -r '.body // ""' <<<"$pr_json")
head_sha=$(jq -r '.head.sha' <<<"$pr_json")
if ! files=$(with-proxy gh api --paginate "repos/$repo/pulls/$pr/files" \
    --jq '.[].filename'); then
  echo "changed-file fetch FAILED -- cannot decide whether this is KVM" >&2
  exit 2
fi
files_kvm_status=0
grep -qiE 'kvm' <<<"$files" || files_kvm_status=$?
labels_kvm_status=0
grep -Fixq kvm <<<"$labels" || labels_kvm_status=$?
case "$files_kvm_status" in
  0 | 1) ;;
  *) echo "KVM changed-file lookup FAILED (grep exit $files_kvm_status)" >&2; exit 2 ;;
esac
case "$labels_kvm_status" in
  0 | 1) ;;
  *) echo "KVM label lookup FAILED (grep exit $labels_kvm_status)" >&2; exit 2 ;;
esac
if [ "$files_kvm_status" -eq 0 ] || [ "$labels_kvm_status" -eq 0 ]; then
  is_kvm=true
else
  is_kvm=false
fi
pr_comments_file=$(mktemp)
trap 'rm -f "$pr_comments_file"' EXIT
if ! with-proxy gh api "repos/$repo/issues/$pr/comments?per_page=100" \
    --paginate --slurp | jq -ce 'add // []' >"$pr_comments_file"; then
  echo "comment fetch FAILED -- this is not a pass" >&2
  exit 2
fi

PR_NUMBER="$pr" PR_LABELS="$labels" PR_BODY="$body" PR_IS_KVM="$is_kvm" \
PR_HEAD_SHA="$head_sha" PR_COMMENTS_FILE="$pr_comments_file" \
  bash scripts/core-review-protocol-lint.sh
```

The inputs are required and the script refuses rather than guessing: an unset
`PR_LABELS` or `PR_BODY` exits 2; for a labeled PR, a missing `PR_HEAD_SHA` or a
missing or unreadable `PR_COMMENTS_FILE`/`PR_COMMENTS_JSON` exits 1. Pass an
explicit empty string only to mean "genuinely none". The file form is used
because a full comment history can exceed the per-variable size limit.

**Do not arm the gate.** The lint and the merge gate stay hand-run: no
`pull_request` trigger on `merge-gate.yml` and no `required_status_checks`
rule. A trigger without a rule, or a rule without a trigger, yields a check that
gates nothing or a requirement nothing can satisfy. Both layers or neither, and
that pairing is an owner decision.

## Landing

Main is gated: a change lands only through the project's validated landing
path, which validates the exact head it lands. A `locally-validated` label,
command exit, or comment is only a cache of evidence. GitHub checks are
supplemental; a genuine failure they reveal still blocks.

Land only when the task authorizes it, required adversarial review is resolved,
and the semantic verifier accepts the current head. Use the serialized tracked
landing path, never `--admin`, then fetch main and prove ancestry. Human review
happens after landing and corrections fix forward.
