# Main branch landing signals

An exact-head green branch may land directly by fast-forward. GitHub's merge
queue and the `merge-gate-v4` workflow remain available as advisory signals,
but neither is a required main-branch status check.

The advisory `merge-gate-v4` job passes when either:

- the diagnostic aggregate jobs in the latest `.github/workflows/ci-portable.yml` and
  `.github/workflows/ci-privileged.yml` runs for the exact pull request head
  both completed successfully; or
- the pull request has the `locally-validated` label and an exact-head receipt
  whose immutable content proves a counted, clean, full `./scripts/validate.rs` pass.

Every check reader uses three outcomes:

- **PASSED**: a terminal success result. This is the only hosted state reported
  as green.
- **FAILED**: a terminal `failure`, `timed_out`, `error`, or `startup_failure`.
  It is an attributable hosted red, not a server-side veto of an independently
  qualifying local-green fast-forward.
- **NO_RESULT**: cancelled, skipped, neutral, stale, action-required, active,
  absent, or unknown. It prevents an advisory green without blocking an
  independently qualifying local-green fast-forward.

An exact-head full local PASSED record is landing authority under the owner
policy; it does not rewrite or reinterpret hosted results.

## Status consumer inventory

Parent `ci-hub/check_outcome.py` is the check-status authority. The gate fetches
it at the exact parent authority commit, verifies its digest, and executes it. Hermit's shell,
PR-status, lander/DAG, and pinned landing-planner entry points share one
`agent-utils` adapter; it locates and digest-checks the same parent source rather
than carrying a conclusion table.
`scripts/check-merge-gate-policy.sh` rejects a duplicate jq table or a consumer
that bypasses the parent authority. The state table is enforced at every
decision surface:

- `.github/workflows/merge-gate.yml` classifies portable, privileged, demo,
  review-protocol, and validation-invalidation results before admission.
- `scripts/pr_status.py` reports advisory-check rollups and main workflow
  history without counting NO_RESULT as red or green.
- `scripts/pr-dag-health.sh` and the pinned `agent-utils` landing planner report
  `merge-gate-v4` as supplemental hosted evidence; direct fast-forward landing
  follows the exact-head local authority instead.
- Parent `ci-hub` uses its canonical `check_outcome.py` model in landing,
  validate-status, health, remediation, and history consumers.

Two consumers are intentionally not generic admission classifiers.
`ci-portable.yml` accepts a skipped internal shard only after affected-test
selection proves that shard deselected; a cancelled selected shard still fails
the aggregate. `ci-portable-autoretry.yml` retains its historical cancellation
consumer, but its only current trigger is manual and a dispatch has no
workflow-run payload, so it creates no automatic retry or admission result.

The merge-gate workflow retains historical jobs for removing
`locally-validated` after a pull-request head change and re-running after CI or
label changes. Those events are not active triggers; the workflow is manual and
advisory. Manual tooling that strips the label must record a durable evidence
comment (see "Validation-evidence trail" below) so the record of what was
validated is never lost.

The job first verifies that its workflow file has the exact Git blob registered
in the server-side `MERGE_GATE_V4_BLOB` variable. This rejects accidental drift
inside the advisory workflow. The context name remains versioned so consumers
can distinguish incompatible classifier semantics; it is not required by the
main-branch ruleset.

This is not a cryptographic attestation of PR-owned YAML. A deliberate workflow
edit can delete the blob-check step while retaining the v4 job name, and both
runs use the same GitHub Actions integration. User-owned repositories cannot
use GitHub's pinned required-workflow rule, so gate-policy PRs must remain an
escalated adversarial-review class. A dedicated trusted GitHub App signer (or an
organization-owned required workflow) is needed to close that stronger threat.

Add an approved pull request to the queue with:

```bash
with-proxy gh pr merge <number> --repo rrnewton/REPOSITORY --auto --merge
```

Replace `REPOSITORY` with `hermit` or `reverie`.

## Local validation

A full green `./scripts/validate.rs` run writes its local ledger row on exit and then
delegates to the parent `ci-hub apply-local-label`. The applier requires that
exact head to have a clean, commit-anchored, full-selection PASS with a nonzero
executed-test count, hashes the referenced log, and publishes the selected row
on `rrnewton/dev-hermit:validation-receipts`. Only after that immutable receipt
exists does it post the binding comment and apply `locally-validated`.
Publication or GitHub failures fail closed; the command can be run manually to
backfill a validated head.

Use `./scripts/validate.rs --no-label-pr` or `VALIDATE_LABEL_PR=0 ./scripts/validate.rs`
when a green run must not update GitHub.

The label is an exact-head local validation signal, not a partial-test waiver.
Apply it only through a full green validator run on the exact pull request head.
Without that local receipt, the advisory merge-gate can report green only when
both hosted jobs pass. It retains a hosted failure as a red diagnostic, but that
red does not override qualifying local evidence or block a direct fast-forward.

## Validation-evidence trail

The label is only a cache of a validation receipt; it cannot create evidence.
Parent `ci-hub/validation/verify_receipt.sh` is the receipt authority used by
the gate. The gate fetches it from the exact parent authority commit and verifies
its digest rather than running PR-controlled verifier code. It resolves the marker's receipt
commit, proves that commit belongs to the receipt branch, reads the exact path
at that commit, recomputes SHA-256, and then validates the exact-head counted
ledger row. A well-shaped comment without that backing receipt is refused.

Stripping `locally-validated` must never silently erase the record of what was
validated. Two symmetric comments preserve it:

- **Add time.** The parent `ci-hub apply-local-label` authority requires a
  qualifying local ledger row, preserves and hashes its log, publishes an
  immutable receipt, comments with a machine-parseable
  `<!-- locally-validated-receipt commit=... path=... sha256=... -->` marker,
  and only then applies the label.
- **Strip time.** `scripts/label-strip-evidence.sh` posts a comment recording
  the strip (validated SHA, new head, reason, timestamp) and quotes the matching
  add-time evidence comment. It is best-effort and always exits 0, so it can
  never fail a gate job or block landing.

The live strip path must leave the trail; the automated paths below are retained
only as historical workflow logic:

1. **Historical on-push strip.** The `invalidate-local-validation` job in
   `.github/workflows/merge-gate.yml` contains handling for
   `pull_request: synchronize`, but no pull-request trigger invokes it.
2. **Live manual agent/tooling strip.** A human or agent removing the label
   (`gh pr edit --remove-label locally-validated`, `gh api DELETE
   .../labels/locally-validated`, or a remove+add re-fire toggle) must run
   `scripts/label-strip-evidence.sh --pr <n> --validated-sha <sha> [--remove]`
   so the evidence is preserved. The `--remove` flag also strips the label.
3. **Historical evidence-mutation handling.** The workflow contains logic to
   revalidate edited or deleted receipt comments, publish advisory checks, and
   remove `locally-validated`, but no issue-comment trigger invokes it. A manual
   dispatch does not carry that event payload. The retained code documents the
   old behavior; it is not current enforcement.

The receipt is remotely readable from every gate runner and immutable at its
referenced commit, unlike a devbig014-local ledger path. The local applier reads
the ledger and log before publication; the gate verifies the receipt content
digest, including the publisher's asserted log path and digest, but cannot
reopen and re-hash the host-local log. Shared `rrnewton` credentials still do not
provide individual signer identity, so a holder could deliberately publish a
false receipt. This prevents accidental label/comment forgery; malicious-token
resistance needs a dedicated signing identity.

The gate fetches its verifier from immutable parent commit `f9e61247` and checks
the script's SHA-256 before execution. It never executes a verifier from the PR
under test; otherwise a PR could authorize itself without changing the gate
workflow.

## Landing from a linked worktree: two false negatives

Most agents land from `git worktree` checkouts, where two ordinary habits silently
report the wrong thing. Both cost real time on 2026-08-25.

### `.git/rebase-merge` does not exist in a linked worktree

Checking for an interrupted rebase with

```bash
ls .git/rebase-merge          # FALSE NEGATIVE in a linked worktree
```

always reports "no rebase in progress", because a linked worktree keeps its
per-worktree state elsewhere. The real path is

```bash
ls "$(git rev-parse --git-dir)/rebase-merge"        # correct anywhere
# e.g. .git/worktrees/<worktree-name>/rebase-merge
```

`git rev-parse --git-dir` resolves to the per-worktree directory, so it is right
in both a normal clone and a worktree. Measured: a completed-but-unswept
`rebase-merge` left from an earlier branch blocked every subsequent `git rebase`
in that worktree, while the wrong-path check said there was nothing there.

**Clear it with `git rebase --quit`, not `rm -fr`.** `--quit` abandons the stale
rebase state without moving `HEAD` or touching stashes; the `rm -fr` the git
error message suggests is fine only once you have confirmed what is in the
directory, and its `head-name`/`orig-head`/`onto` files tell you whose rebase it
was before you delete anyone's work.

### A tailed command hides its own failure, the same way a piped push does

`ci-hub/bin/git-push-verified` exists because `git push ... | tail` reports
`tail`'s exit status, so a REJECTED push reads as success. The same trick hides a
failed rebase:

```bash
git rebase origin/main 2>&1 | tail -1     # may print a fragment of an ERROR
```

A rebase that aborts prints a multi-line explanation; tailing it can surface a
harmless-looking last line such as `valuable there.` while the rebase did not
run at all. The tell is that `HEAD` did not move — check that, not the text.

Read the status of a landing command directly, then **verify by content on the
remote**: `git fetch` and compare the blobs you meant to change. Exit status
answers "did the command succeed"; only the remote answers "did the change
arrive".

## Repository settings

The `main` branch rulesets must:

1. keep the legacy check-gating ruleset rule-empty—hosted checks and PR state
   are advisory;
2. disallow non-fast-forward updates; and
3. require linear history (reject merge commits); and
4. disallow branch deletion.

Verify the live rule without mutating it:

```bash
with-proxy scripts/configure-merge-gate-ruleset.sh --check
```

That checker refuses any rule in the check-gating ruleset and `--apply` empties
its rule list while preserving the ruleset envelope. The separate
history-protection ruleset remains the authority for non-fast-forward and
deletion refusal plus linear-history enforcement.

If a stale writer reintroduces a landing rule, run `--apply`; it removes every
rule from the legacy check-gating ruleset and verifies the complete normalized
result.
Each full-object PUT is preceded by a fresh equality check, which detects
policy drift already visible before the write. GitHub exposes no conditional
PUT for this endpoint, so a narrow read-to-write TOCTOU window remains; the
full post-state check detects the resulting mismatch but cannot make the
update atomic.

GitHub's merge queue may still be used as a convenience, but it does not
supersede the owner-authorized direct fast-forward path.
