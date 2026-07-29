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
inventory. CI creates one independently schedulable run node for every bucket.
Six buckets currently contain calibrated blocking workloads:

- `system-utils.toml`
- `data-handling.toml`
- `determinism-stress.toml`
- `language-runtimes.toml`
- `applications.toml`
- `c-programs.toml` (eight calibrated Buck-derived C probes)

Eight additional `*-c.toml`/`c-programs.toml` buckets make 180 more C guests
centrally discoverable. Eight `c-programs.toml` entries have calibrated
standalone build and output contracts and run in blocking CI; the remaining
172 C guests keep `ci = false` until they are calibrated. Buckets without a
calibrated cell still have a CI node that intentionally reports zero cells,
and the correspondence audit proves that this cannot hide a calibrated cell.
Every entry still declares all five modes and every backend exclusion, so
inventory does not silently imply support.

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

Any mode may declare backend-specific guest arguments. The harness appends
these after the guest executable, separately from Hermit's own arguments:

```toml
[test.modes.verify]
ci = false
backends_enabled = ["ptrace", "kvm"]
guest_args = { ptrace = ["multi"], kvm = ["multi"] }
```

Every `guest_args` key must name an enabled backend. Omitted backends receive
no guest arguments.

`naked` must set `ci = false`; it runs only when explicitly selected. A mode
with no enabled backend remains visible with `ci = false` and a reason for
every disabled backend. Regular CI executes only cells with `ci = true`;
run one enabled manual cell with explicit test and mode filters:

```sh
./ci/test_harness.sh run --include-manual --mode verify \
  --test c-programs/add-key-enosys
```

`--include-manual` requires both exact filters so a broad CI command cannot
accidentally pull the uncalibrated corpus into its run plan.
Callers that combine explicit mode/backend filters with CI policy must add
`--ci-only`. This is how `validate.sh quick` avoids expanding the manual C
inventory.

## Inventory and validation

`inventory/test-files.json` classifies every regular file and symlink below
`tests/` with a disposition, owning runner, and per-file justification. The
audit compares the inventory byte-for-byte with filesystem discovery, then
confirms that every manifest program is classified as `manifest-test`. Tests
retained under Cargo, Buck, integration, QEMU, or suite drivers explain the
build flags, arguments, expected results, hardware, or shared setup that their
owner supplies. Each exception names its exact owning runner and the file's
specific role; generic category-only justifications fail review even when the
inventory is mechanically complete.

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
./ci/test_harness.sh build --lane portable --ci-only
./ci/test_harness.sh run --lane portable
./ci/test_harness.sh run --lane portable --category system-utils --ci-only --prebuilt
./ci/test_harness.sh run --mode naked --test system-utils/random-device
```

Both GitHub workflows and `validate.sh` execute the same portable and
privileged DAG files. Each DAG has a manifest guest-build barrier followed by
one structured selector per bucket. `audit-ci` fails if either caller stops
delegating to the shared plans, a bucket node disappears, a command diverges
from its selector, or the aggregate selected cells differ from the ratchet.

## Adding a test

1. Put behavior in a focused shell, C, or Rust source file.
2. Add it to exactly one bucket and declare all five modes.
3. Enable only combinations proven locally; justify every exclusion.
4. Add or update its exact entry in `inventory/test-files.json`.
5. Run `./ci/test_harness.sh validate` and the affected cells.
6. Add a structured DAG node when adding a bucket; validation fails until each
   lane has exactly one node per bucket.

Do not replace a semantic workload with `--help`, `--version`, or a no-op
launcher probe.
