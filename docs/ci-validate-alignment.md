<!--
Copyright (c) Meta Platforms, Inc. and affiliates.
All rights reserved.

This source code is licensed under the BSD-style license found in the
LICENSE file in the root directory of this source tree.
-->

# CI and validate.sh alignment

Hermit CI is partitioned by host capability, not by test duration:

| Lane | Workflow and runner | Local command | Capability contract |
| --- | --- | --- | --- |
| Portable | `ci-portable.yml`, `ubuntu-latest` | `./validate.sh portable-only --no-label-pr` | No PMU counters, CPUID faulting, or KVM |
| Privileged | `ci-privileged.yml`, `[Linux, X64, hermit, pmu]` | `./validate.sh --privileged-only --no-label-pr` | PMU overflow delivery, CPUID faulting, and read/write `/dev/kvm` |

The portable workflow is the required broad product gate. The privileged
workflow is a focused capability sentinel and must finish in less than five
minutes. Long PMU stress, debugger, language-runtime, and application matrices
do not belong in the scarce privileged lane.

## Multi-mode E2E harness

`ci/test_harness.sh` discovers tests from the schema-v2 TOML bucket files under
`tests/e2e/manifests/`. The CI-enabled category set is:

- `system-utils`
- `data-handling`
- `determinism-stress`
- `language-runtimes`
- `applications`

Eight additional C-corpus buckets are centrally discoverable with `ci=false`
until each direct guest's standalone build and output contract is calibrated.
They participate in schema, inventory, and disabled-backend audits without
silently expanding the blocking CI denominator.

Test programs contain no policy annotations. Each central entry declares its
program path, lane, observation tuple, timeout, and all five modes. Mode is the
outer list; each mode partitions its complete backend set between
`backends-enabled` and `backends-disabled`, with a reason for every disabled
backend. `ci/test_harness.sh validate` fails on an invalid partition, stale
inventory, unclassified file under `tests/`, or replay backend other than
ptrace.

The modes have distinct contracts:

| Mode | Contract |
| --- | --- |
| `naked` | Explicit meta-CI only: run three to five times without Hermit and require declared nondeterminism |
| `verify` | Run every allowlisted backend with `--strict --verify` |
| `replay` | Run ptrace `record start --strict --verify` in an isolated recording directory |
| `chaos` | Require cross-seed diversity and exact within-seed reproduction |
| `custom` | Run Hermit with manifest-declared edge-case arguments |

Portable Hermit cells add `--no-virtualize-cpuid` and
`--max-timeslice=disabled`. Every result records the source SHA and dirty bit,
test and binary hashes, effective arguments, relaxations, lane, mode, backend,
duration, and outcome. JSONL, JUnit XML, and a denominator-aware summary are
stored below `target/e2e/` and uploaded by both workflows.

Each cell receives repo-local `HOME`, `XDG_CONFIG_HOME`, fixtures, captures,
and recording directories. Hermit guests use the isolated `/tmp/hermit-e2e`
logical work path so built-in verification cannot leak run-one mutations into
run two. The checked-in XDG seed is under `tests/e2e/xdg-config/`; developer
configuration is never read.

## DAG wiring

`ci/dag/portable.json` has one metadata node and one resource-serialized E2E
node per category. Those nodes depend on `build.workspace`; other build, lint,
documentation, and unit-test nodes retain their existing dependencies.

`ci/dag/privileged.json` contains only:

- the focused Hermit and Detcore build;
- CPUID-faulting validation;
- PMU overflow/skid validation; and
- the KVM E2E shell/environment sentinel.

Both `validate.sh` and GitHub Actions execute these exact DAG files. Use
`ci/run-dag.sh portable ascii` or `ci/run-dag.sh privileged ascii` to audit the
dependency layers without running tests. `ci/test_harness.sh audit-ci` hashes
the ordered step IDs and commands and verifies both callers still delegate to
the shared plans.

## Reconciliation checklist

When adding or changing an E2E test:

1. Put the workload in a focused shell, C, or Rust source file.
2. Add it to exactly one bucket manifest and declare all five modes.
3. Add only locally proven backend combinations to an allowlist.
4. Run `ci/test_harness.sh validate` and inspect `plan --format json`.
5. Run the affected mode/backend cells and retain their JSONL/JUnit results.
6. Update `tests/e2e/manifests/inventory/test-files.json` with its disposition and runner.
7. Update the owning DAG only when a category or capability dependency changes.
8. Never replace a semantic workload with `--help`, `--version`, or a no-op
   launcher probe.
