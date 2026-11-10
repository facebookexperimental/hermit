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

GitHub-hosted portable CI (`.github/workflows/ci-portable.yml`) runs
automatically on pull requests opened by anyone other than the project bot,
from a branch or a fork, once the pull request is out of draft. A run for a
fork pull request waits for a maintainer to approve it in the Actions tab; the
maintainer first reads the pull request's `.github/` changes, because GitHub
runs the workflow files from the pull request itself. The privileged
(self-hosted, PMU/KVM) workflows are not triggered by pull requests: a
maintainer dispatches them after reviewing the change. CI results are
supplemental evidence; local validation (below) is what a reviewer looks for.

## Local Validation Protocol

Every change must be **locally validated before it is pushed**. The single
entry point is:

```bash
./scripts/validate.rs
```

A bare invocation runs selective validation: docs-only changes skip,
a change confined to the DBT crates (`detcore-dbt/**`, `detcore-dbt-cdylib/**`,
or `reverie-dbt/**` in an expanded Reverie pin) runs the common-core nodes
plus every E2E bucket containing a DBT cell, and everything else runs the
full label. It fails safe to the full label when no trustworthy baseline
exists. Scoped kvm, ptrace, liteinst, sabre and e9patch routes are residual
future work, not v1 routes. Run `./scripts/validate.rs --all` for the
complete suite.

`scripts/validate.rs portable-only` is the local mirror of the required
`.github/workflows/ci-portable.yml` workflow: it runs the same build, lint,
format, documentation, and test matrix. The focused privileged capability
contract is reproduced with `ci/run-dag.sh privileged -j 2`.
The exact step-by-step mapping between the two — and any sanctioned
host-capability differences — is documented in
[`docs/ci-validate-alignment.md`](docs/ci-validate-alignment.md). Do not add a
test to only one side: a test that gates CI must be reproducible with
`scripts/validate.rs`, and vice versa.

For a predictable portable iteration loop that mirrors GitHub-managed portable CI, run:

```bash
VALIDATE_LEVEL=portable-only ./scripts/validate.rs
```

`./scripts/validate.rs --portable` is equivalent, while `./scripts/validate.rs --quick` selects
the smaller ptrace smoke suite. These modes provide fast feedback; use `full`
for the complete hardware-dependent local gate when the host supports it.

Protocol for every PR:

1. Run `./scripts/validate.rs` on your branch and make it pass. The final line reads
   `✅ Validation summary (N passed, 0 failed; …)`. If a check cannot run on
   your host (for example, tests that require PMU access or mount namespaces),
   say so explicitly in the PR description — state the command, the host
   limitation, and what you observed. Never silently skip a check or weaken a
   hardware-sensitive assertion to make a local VM green.
2. When you add, remove, or rename a test, update `scripts/validate.rs` and the
   owning CI workflow in the same PR so they stay in lockstep, and update the
   mapping table in `docs/ci-validate-alignment.md`. See the "Reconciliation
   checklist for test-adding PRs" in that document.
3. Inside the maintainers' `dev-hermit` harness, a full green
   `scripts/validate.rs --all` delegates to `ci-hub apply-local-label` after its
   ledger row is written. Do not add **`locally-validated`** by hand there: the
   applier re-reads the counted exact-head row, hashes its log, publishes an
   immutable receipt, and only then comments and labels. The command can also
   be run manually to backfill a validated head. Re-run validation after any
   subsequent push; the push invalidates both the prior receipt and label.
4. **Outside that harness** (a plain clone of this repository, including
   contributors' automation), `./scripts/validate.rs` runs the same plan
   (`--show-plan` lists it) and records its result in the standalone ledger
   under `ci/validate-ledger/`, but it cannot publish a receipt or apply the
   label, and says so ("not applying locally-validated"). The pinned-root
   nodes need Podman and the locally built pinned image (`ci/hermetic/README.md`,
   `ci/hermetic/build-image.sh`); PMU and KVM cells need a host that has them.
   To have the run recorded, post one pull-request comment headed
   `Local validation` that contains:
   - the exact 40-hex head commit you validated;
   - the command and level (`./scripts/validate.rs`, or `portable-only` when
     the host cannot run the full level);
   - the run's final summary line and its `FINAL_VALIDATE_STATUS:` line;
   - the host's relevant capabilities (kernel version, PMU yes/no, KVM yes/no)
     and any check that could not run;
   - the standalone ledger row the run appended under `ci/validate-ledger/`.

   A maintainer checks that the commit is the pull request's current head and
   the status is a pass, then applies `locally-validated`. A later push
   invalidates it, as above.

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

## Coding Agents

Agents working in the development workspace should also read the
[agent quick start](docs/AGENT_QUICKSTART.md) for the workspace's bounded-run
wrapper and experiment conventions.

## License

By contributing to Hermit, you agree that your contributions will be
licensed under the LICENSE file in the root directory of this source tree.
