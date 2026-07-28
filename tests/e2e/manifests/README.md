<!--
Copyright (c) Meta Platforms, Inc. and affiliates.
All rights reserved.

This source code is licensed under the BSD-style license found in the
LICENSE file in the root directory of this source tree.
-->

# Centralized e2e test manifests (schema v2)

These TOML files are the load-bearing policy source for Hermit's executable
end-to-end tests. Test programs contain behavior only; lane, mode, backend,
timeout, build flags, observation policy, and exclusion reasons belong here.
`ci/test_harness.sh` loads them through the structured Rust parser in
`ci/manifest-plan`.

The 13 manifests separate calibrated blocking cells from discoverable migration
inventory. CI currently shards the five calibrated workload buckets:

- `system-utils.toml`
- `data-handling.toml`
- `determinism-stress.toml`
- `language-runtimes.toml`
- `applications.toml`

Eight additional `*-c.toml`/`c-programs.toml` buckets make 180 more C guests
centrally discoverable. Their ptrace verify cells are enabled for explicit
mode selection but set `ci = false` until their standalone build and output
contracts are calibrated. They still declare all five modes and every backend
exclusion, so inventory does not silently imply support.

## Schema contract

Every `[[test]]` names either a repo-relative `program` or a `direct` shell
command. Program extensions select the runner:

- `.sh`: execute the existing `--prepare`/`--run` protocol directly;
- `.c`: compile implicitly with `cc` plus optional `build.cflags`;
- `.rs`: compile implicitly with `rustc` plus optional `build.rustflags`.

`MODE` is always the outer axis. Every entry declares exactly these five
tables: `verify`, `chaos`, `replay`, `naked`, and `custom`. Each table has a
`backends_enabled` list and a `backends_disabled` table. The two must form a
complete, disjoint partition and every disabled backend needs a nonempty WHY.
For non-naked modes the axis is `ptrace`, `dbi`, `kvm`, `sabre`, and
`liteinst`; naked partitions only `native`.

```toml
[test.modes.verify]
ci = true
backends_enabled = ["ptrace"]
[test.modes.verify.backends_disabled]
dbi = "DBI coverage is owned by its backend parity partition"
kvm = "KVM requires the privileged runner"
sabre = "SaBRe requires its external runtime"
liteinst = "LiteInst coverage is owned by its compatibility partition"
```

The mode contracts are:

| Mode | Contract |
| --- | --- |
| `verify` | Run each enabled backend with `hermit run --strict --verify` |
| `chaos` | Search declared seeds and require cross-seed diversity plus exact within-seed reproduction |
| `replay` | Run ptrace `record start --strict --verify` in an isolated recording directory |
| `naked` | Opt-in meta-CI only; run natively three to five times and require declared variation |
| `custom` | Run declared edge-case Hermit arguments and require three to five identical observations |

`naked` must set `ci = false`; it runs only when explicitly selected. A mode
with no enabled backend remains visible with `ci = false` and a reason for
every disabled backend. Regular CI executes only cells with `ci = true`;
selecting a mode explicitly also exposes enabled manual cells:

```sh
./ci/test_harness.sh run --mode verify --test c-programs/add-key-enosys
```

## Inventory and validation

`inventory/test-files.json` classifies every regular file and symlink below
`tests/` with a disposition, owning runner, and per-file justification. The
audit compares the inventory byte-for-byte with filesystem discovery, then
confirms that every manifest program is classified as `manifest-test`. Tests
retained under Cargo, Buck, integration, QEMU, or suite drivers explain the
build flags, arguments, expected results, hardware, or shared setup that their
owner supplies.

`ci/expected-e2e-plan.json` ratchets the exact blocking cells. Adding, removing,
or reclassifying a `ci=true` cell fails validation until the expected plan is
updated in the same review.

Use the load-bearing entrypoints:

```sh
cargo run -p hermit-manifest-plan -- --format text
./ci/test_harness.sh validate
./ci/test_harness.sh plan --format json
./ci/test_harness.sh audit-gaps --format json
./ci/test_harness.sh audit-ci
./ci/test_harness.sh run --lane portable
./ci/test_harness.sh run --mode naked --test system-utils/random-device
```

Both GitHub workflows and `validate.sh` execute the same portable and
privileged DAG files. `audit-ci` fingerprints those ordered commands and fails
if either caller stops delegating to the shared plans.

## Adding a test

1. Put behavior in a focused shell, C, or Rust source file.
2. Add it to exactly one bucket and declare all five modes.
3. Enable only combinations proven locally; justify every exclusion.
4. Add or update its exact entry in `inventory/test-files.json`.
5. Run `./ci/test_harness.sh validate` and the affected cells.
6. Update a DAG only when the category or capability dependency changes.

Do not replace a semantic workload with `--help`, `--version`, or a no-op
launcher probe.
