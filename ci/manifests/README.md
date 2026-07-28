<!--
Copyright (c) Meta Platforms, Inc. and affiliates.
All rights reserved.

This source code is licensed under the BSD-style license found in the
LICENSE file in the root directory of this source tree.
-->

# Central test manifests

The JSON files in this directory are the policy source for Hermit's executable
end-to-end tests. Test programs contain behavior only; lane, mode, backend,
timeout, and observation policy belong here.

The manifests are split by workload bucket so reviews do not require editing a
single monolith:

- `system-utils.json`
- `data-handling.json`
- `determinism-stress.json`
- `language-runtimes.json`
- `applications.json`

Every test uses schema 2. `modes` is always the outer list. Each of the five
modes (`verify`, `chaos`, `replay`, `naked`, and `custom`) contains explicit
`backends-enabled` and `backends-disabled` lists. Every disabled backend has a
nonempty `why`; the validator rejects duplicate, missing, or overlapping
classifications.

`naked` is a meta-CI control, not a regular CI mode. It must set `ci` to false,
uses the `native` backend, and runs three to five times. Select it explicitly:

```sh
./ci/test_harness.sh run --mode naked --test system-utils/random-device
```

Regular CI runs only cells with `ci: true`. C programs are compiled implicitly
into the cell fixture directory. Shell programs are executed directly with the
declared prepare and run arguments.

## Test-tree inventory

`inventory/test-files.json` gives every file under `tests/` one explicit
disposition, runner, and justification. `ci/test_harness.sh validate` compares
that list byte-for-byte with current repository discovery, so adding or
removing a test file without classifying it fails CI. Files not represented as
direct manifest tests must explain why their build flags, arguments, expected
results, hardware, or shared setup remain owned by a Cargo, Buck, QEMU, or
category driver.

## Load-bearing entrypoints

```sh
./ci/test_harness.sh validate       # schema, inventory, and CI correspondence
./ci/test_harness.sh plan           # regular required E2E cells
./ci/test_harness.sh audit-gaps     # unvalidated backend cells
./ci/test_harness.sh audit-ci       # exact validate/GitHub DAG fingerprints
./validate.sh portable-only         # ci/dag/portable.json
./validate.sh --privileged-only     # ci/dag/privileged.json
```

GitHub workflows and local validation execute the same two DAG files. The
correspondence audit fails if either side stops consuming those shared plans.
