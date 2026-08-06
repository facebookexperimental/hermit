---
name: ci-debugging
description: "Debug Hermit/Reverie validation failures from exact-SHA receipts: inspect the failed DAG node, iterate on only that node, then obtain one clean full-profile receipt at the final head. Use whenever local validation or supplemental GitHub CI is red."
---

# Debug validation from exact evidence

## Establish the authority first

Inside the dev-hermit harness, query the current PR head through the parent's
semantic verifier:

```bash
./ci-hub/ci-hub validate-status <40-hex-head-SHA>
```

Run that command from the dev-hermit root. A clean, counted, full-profile
receipt for the exact head is the local landing authority. A
`locally-validated` label, copied comment, raw command exit, or receipt for an
earlier SHA is only a cache. GitHub workflows are delayed supplemental signal:
inspect a genuine failure, but do not wait for them merely to duplicate a
qualifying local receipt.

Record the exact SHA, profile, discovered/selected/executed/filtered/failure
counts, failing node names, and durable log path. A green with zero executed
tests or incomplete declared coverage is a no-result.

## Reproduce only the failing node

The portable and privileged lanes are DAGs under `ci/dag/`. Each `group.job`
tag names one independently runnable command. Use the receipt or DAG-runner log
to identify the failing tag, build prerequisites once, and then iterate only on
that tag:

```bash
./validate.sh --only portable test.sabre_examples
ci/run-node.sh portable test.sabre_examples
ci/run-node.sh portable e2e.manifest_backend_parity_c,test.dbt_parity
```

Prefer `validate.sh --only` as the user-facing entrypoint. `ci/run-node.sh` is
useful when debugging the node runner itself. Do not rerun the full profile for
every edit, and do not claim a focused node pass as full validation.

If the failure appeared only in supplemental GitHub CI, retrieve the exact job
log and map it back to its current DAG node. Reproduce locally when the command
and required capability exist. Hardware, kernel, or runner-only failures must
be reported as such; they are not permission to weaken assertions.

## Classify before changing code

- A deterministic assertion, compile error, manifest check, or stable node
  failure is normally a product or test defect. Reproduce it at the same SHA.
- A timeout needs load and duration evidence before it is called a product
  regression. Compare the same node under a quiet admitted run; never erase a
  correctness check to accommodate contention.
- A failure shared across many PRs should be fixed once at its common source.
  Do not create per-PR churn without proving each branch contributes.
- A stale receipt, dirty worktree, wrong profile, skipped node, or zero-test run
  is an evidence defect. Repair the run rather than relabeling it green.

For non-KVM determinism or parity failures, use `--verify-strict` and require a
`--verify-json` verdict with `bitwise_parity: true`. That compares exit status,
stdout, and stderr byte-for-byte and INFO events under `BitwiseInfoV1`, which
retains numeric payloads while removing only wall-clock prefixes and
ordinalizing host addresses. Default `--verify`, KVM's output-only fallback,
and more aggressively normalized logs cannot establish L2 INFO parity; they
may only help localize the first divergence.

## Requalify the final head once

After focused tests pass, commit the fix and request one full run through the
dev-hermit parent's current `ci-hub validate-lock` admission path. The admitted
command must run in the registered clean worktree at the exact branch head.
Do not bypass the fleet lock with a private detached launcher.

When the run finishes:

1. Query `ci-hub validate-status` again for the full 40-hex head SHA.
2. Verify nonzero execution, complete profile coverage, zero failures, a clean
   tree, and the durable log/receipt linkage.
3. Update the PR with exact commands and results. Treat labels as derived cache.
4. If the head changes for any reason, invalidate the evidence and requalify.

The PR author owns this loop through landing: fix review findings, rebase when
needed, validate the new head, and prove the landed commit on freshly fetched
`main`.

## Related skills

- [hermit-debugging](../hermit-debugging/SKILL.md) for guest execution failures.
- [deadlock-debugging](../deadlock-debugging/SKILL.md) for hangs and no-progress states.
- [determinism-regression-debugging](../determinism-regression-debugging/SKILL.md)
  for a regression with a known-good reference.
- [repo-cleanliness](../repo-cleanliness/SKILL.md) before committing the fix.
