# Contributing to Hermit

We want to make contributing to this project as easy and transparent as
possible.

## Our Development Process

Hermit is currently developed in Meta's internal repositories and then
exported out to GitHub by a Meta team member; however, we invite you to
submit pull requests as described below.

## Pull Requests

We actively welcome your pull requests.

1. Fork the repo and create your branch from `main`.
2. If you've added code that should be tested, add tests.
3. If you've changed APIs, update the documentation.
4. Ensure the test suite passes.
5. Make sure your code lints.
6. If you haven't already, complete the Contributor License Agreement ("CLA").

## Local Validation Protocol

Every change must be **locally validated before it is pushed**. The single
entry point is:

```bash
./validate.sh
```

`validate.sh` is the local mirror of the GitHub Actions workflow
(`.github/workflows/ci.yml`): it runs the same build, lint, format, doc, and
test matrix, in the same modes, so a green local run predicts a green CI run.
The exact step-by-step mapping between the two — and any sanctioned
host-capability differences — is documented in
[`docs/ci-validate-alignment.md`](docs/ci-validate-alignment.md). Do not add a
test to only one side: a test that gates CI must be reproducible with
`validate.sh`, and vice versa.

Protocol for every PR:

1. Run `./validate.sh` on your branch and make it pass. The final line reads
   `✅ Validation summary (N passed, 0 failed; …)`. If a check cannot run on
   your host (for example, tests that require PMU access or mount namespaces),
   say so explicitly in the PR description — state the command, the host
   limitation, and what you observed. Never silently skip a check or weaken a
   hardware-sensitive assertion to make a local VM green.
2. When you add, remove, or rename a test, update **both** `ci.yml` and
   `validate.sh` in the same PR so they stay in lockstep, and update the
   mapping table in `docs/ci-validate-alignment.md`. See the "Reconciliation
   checklist for test-adding PRs" in that document.
3. Once `validate.sh` passes, add the **`locally-validated`** label to the PR.
   The label is a claim that the author ran `./validate.sh` to green on this
   exact revision; it tells reviewers the local gate has been satisfied.
   Re-run `validate.sh` and re-apply the label after any subsequent push that
   changes code.

A PR that changes code but is missing the `locally-validated` label — or whose
description does not account for any check that could not run locally — is not
ready for review.

## Contributor License Agreement ("CLA")

In order to accept your pull request, we need you to submit a CLA. You only
need to do this once to work on any of Meta's open source projects.

Complete your CLA here: <https://code.facebook.com/cla>

## Issues

We use GitHub issues to track public bugs. Please ensure your description is
clear and has sufficient instructions to be able to reproduce the issue.

Meta has a [bounty program](https://www.facebook.com/whitehat/) for the safe
disclosure of security bugs. In those cases, please go through the process
outlined on that page and do not file a public issue.

## Coding Style

Follow the automatic `rustfmt` configuration.

## License

By contributing to Hermit, you agree that your contributions will be
licensed under the LICENSE file in the root directory of this source tree.
