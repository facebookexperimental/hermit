# Main branch landing signals

An exact-head green branch may land directly by fast-forward. GitHub's merge
queue and the `merge-gate-v4` workflow remain available as advisory signals,
but neither is a required main-branch status check.

The advisory `merge-gate-v4` job passes when either:

- the authoritative jobs in the latest `.github/workflows/ci-portable.yml` and
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
the aggregate. `ci-portable-autoretry.yml` consumes cancellation as a trigger
to create a new result and never treats it as pass or failure.

The workflow removes `locally-validated` whenever the pull request head
changes. It also re-runs the gate after CI completes and on label changes, so a
premature pending-CI failure converges without closing and reopening the pull
request. Every strip records a durable evidence comment (see
"Validation-evidence trail" below) so the record of what was validated is never
lost.

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

The label is an alternate merge admission signal, not a partial-test waiver.
Apply it only through a full green validator run on the exact pull request head.
Without that local receipt, the hosted leg requires both the portable and
privileged jobs to pass. A hosted failure is never overridden by local evidence.

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

Known strip paths — all must leave the trail:

1. **Automated on-push strip.** The `invalidate-local-validation` job in
   `.github/workflows/merge-gate.yml` deletes the label on
   `pull_request: synchronize` and then calls `label-strip-evidence.sh`.
2. **Manual agent/tooling strip.** A human or agent removing the label
   (`gh pr edit --remove-label locally-validated`, `gh api DELETE
   .../labels/locally-validated`, or a remove+add re-fire toggle) must run
   `scripts/label-strip-evidence.sh --pr <n> --validated-sha <sha> [--remove]`
   so the evidence is preserved. The `--remove` flag also strips the label.
3. **Evidence mutation.** Editing or deleting a PR comment revalidates the
   current exact-head receipt. If no dereferenceable receipt remains, the
   workflow publishes failing `merge-gate-v4` and retired `merge-gate-v3`
   checks at the exact head, then removes `locally-validated`. The checks keep
   advisory history truthful but are not branch-protection requirements.
   Same-repository branches explicitly dispatch a new exact-head gate because
   label changes made with `GITHUB_TOKEN` do not recursively trigger another
   workflow. Fork heads cannot be used as base-repository workflow-dispatch
   refs; their advisory checks remain until a new receipt and label re-fire the
   pull-request gate.

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
