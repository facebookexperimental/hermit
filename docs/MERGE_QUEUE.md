# Main branch merge queue

Pull requests into `main` land through GitHub's merge queue. The queue creates
a temporary commit against the current `main` tip, preventing a stale pull
request head from bypassing changes that landed ahead of it.

The required check is `merge-gate`. It passes when either:

- the latest `.github/workflows/ci.yml` run for the exact pull request head
  completed successfully; or
- the pull request has the `locally-validated` label from a fully green
  `./validate.sh` run.

The workflow removes `locally-validated` whenever the pull request head
changes. It also re-runs the gate after CI completes and on label changes, so a
premature pending-CI failure converges without closing and reopening the pull
request.

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

## Repository settings

The `main` branch ruleset must:

1. require pull requests and linear history;
2. require the `merge-gate` status check;
3. require GitHub's merge queue; and
4. disallow force pushes and branch deletion.

Enable auto-merge in the repository so `gh pr merge --auto --merge` can queue
eligible pull requests. Do not require the host-dependent CI job separately;
the gate owns the documented CI-or-local-validation policy.
