# Main branch merge queue

Pull requests into `main` land through GitHub's merge queue. The queue creates
a temporary commit against the current `main` tip, preventing a stale pull
request head from bypassing changes that landed ahead of it.

The required policy is `.github/workflows/merge-gate.yml` from `main`. Its
`merge-gate` job passes when either:

- the latest `.github/workflows/ci-portable.yml` run for the exact pull request
  head completed successfully; or
- the pull request has the `locally-validated` label **and** an exact-head
  comment that resolves to an immutable receipt containing a counted, clean,
  full-pass ledger row and the validation log's SHA-256 digest.

The workflow removes `locally-validated` whenever the pull request head
changes. It also re-runs the gate after CI completes and on label changes, so a
premature pending-CI failure converges without closing and reopening the pull
request. Every strip records a durable evidence comment (see
"Validation-evidence trail" below) so the record of what was validated is never
lost.

The ruleset pins the required workflow to `refs/heads/main`. A
`workflow_dispatch` run from a pull request branch is diagnostic only: even if
its identically named `merge-gate` job succeeds, it cannot satisfy the required
workflow. This prevents an old pull request from authorizing itself with an
older, weaker copy of the gate YAML.

Add an approved pull request to the queue with:

```bash
with-proxy gh pr merge <number> --repo rrnewton/REPOSITORY --auto --merge
```

Replace `REPOSITORY` with `hermit` or `reverie`.

## Local validation

A full green `./validate.sh` run writes its local ledger row on exit and then
delegates to the parent `ci-hub apply-local-label`. The applier requires that
exact head to have a clean, commit-anchored, full-selection PASS with a nonzero
executed-test count, hashes the referenced log, and publishes the selected row
on `rrnewton/dev-hermit:validation-receipts`. Only after that immutable receipt
exists does it post the binding comment and apply `locally-validated`.
Publication or GitHub failures fail closed; the command can be run manually to
backfill a validated head.

Use `./validate.sh --no-label-pr` or `VALIDATE_LABEL_PR=0 ./validate.sh`
when a green run must not update GitHub.

The label is an alternate merge admission signal, not a partial-test waiver.
Apply it only through a full green validator run on the exact pull request head.
The privileged workflow remains an independent bonus signal and is not a merge
admission requirement.

## Validation-evidence trail

Stripping `locally-validated` must never silently erase the record of what was
validated. Two symmetric comments preserve it:

- **Add time.** `ci-hub apply-local-label` posts a comment ending in
  `<!-- locally-validated-receipt commit=... path=... sha256=... -->`. The gate
  dereferences that exact Git commit and path, verifies the content hash, and
  requires the embedded ledger row to name the exact PR head, a clean full pass,
  and a nonzero executed-test count. A perfect-looking marker pointing at no
  receipt is refused.
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
   workflow first publishes a failing required `merge-gate` check at the exact
   head, then removes `locally-validated`. Same-repository branches explicitly
   dispatch a new exact-head gate because label changes made with `GITHUB_TOKEN`
   do not recursively trigger another workflow. A dispatch failure therefore
   remains blocked by the already-published failure. Fork heads cannot be used as
   base-repository workflow-dispatch refs; their failing check remains until a
   new receipt and label re-fire the pull-request gate. There remains a
   narrow race if a merge completes before the edit/delete event is processed.

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

The `main` branch ruleset must:

1. require pull requests and linear history;
2. require `.github/workflows/merge-gate.yml` from this repository at
   `refs/heads/main` using GitHub's required-workflow rule, never a bare status
   context named `merge-gate`;
3. require GitHub's merge queue; and
4. disallow force pushes and branch deletion.

Verify the live rule without mutating it:

```bash
with-proxy scripts/configure-merge-gate-ruleset.sh --check
```

The coordinator may reconcile the live ruleset with `--apply`. The command
refuses to discard any unexpected required status context.

Enable auto-merge in the repository so `gh pr merge --auto --merge` can queue
eligible pull requests. Do not require the host-dependent CI job separately;
the gate owns the documented CI-or-local-validation policy.
