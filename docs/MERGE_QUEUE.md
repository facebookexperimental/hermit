# Main branch merge queue

Pull requests into `main` land through GitHub's merge queue. The queue creates
a temporary commit against the current `main` tip, preventing a stale pull
request head from bypassing changes that landed ahead of it.

The required check is `merge-gate`. It passes when either:

- the latest `.github/workflows/ci-portable.yml` run for the exact pull request
  head completed successfully; or
- the pull request has the `locally-validated` label from a fully green
  `./validate.sh` run.

The workflow removes `locally-validated` whenever the pull request head
changes. It also re-runs the gate after CI completes and on label changes, so a
premature pending-CI failure converges without closing and reopening the pull
request. Every strip records a durable evidence comment (see
"Validation-evidence trail" below) so the record of what was validated is never
lost.

Add an approved pull request to the queue with:

```bash
with-proxy gh pr merge <number> --repo rrnewton/REPOSITORY --auto --merge
```

Replace `REPOSITORY` with `hermit` or `reverie`.

## Local validation

A full green `./validate.sh` run automatically creates and applies the
`locally-validated` label to the current branch's pull request. Set
`PR_NUMBER=<number>` when branch-based detection is unavailable. GitHub CLI,
authentication, proxy, missing-PR, and label-edit failures are warnings and do
not change validation's exit status.

Use `./validate.sh --no-label-pr` or `VALIDATE_LABEL_PR=0 ./validate.sh`
when a green run must not update GitHub.

The label is an alternate merge admission signal, not a partial-test waiver.
Apply it only through a full green validator run on the exact pull request head.
The privileged workflow remains an independent bonus signal and is not a merge
admission requirement.

## Validation-evidence trail

Stripping `locally-validated` must never silently erase the record of what was
validated. Two symmetric comments preserve it:

- **Add time.** A green `./validate.sh` posts an evidence comment (commit SHA,
  profile, results, host, durable log path) ending in a machine-parseable marker
  `<!-- locally-validated-evidence sha=... -->`. This is the safety net: it
  survives even if a strip path forgets to comment.
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

## Repository settings

The `main` branch ruleset must:

1. require pull requests and linear history;
2. require the `merge-gate` status check;
3. require GitHub's merge queue; and
4. disallow force pushes and branch deletion.

Enable auto-merge in the repository so `gh pr merge --auto --merge` can queue
eligible pull requests. Do not require the host-dependent CI job separately;
the gate owns the documented CI-or-local-validation policy.
