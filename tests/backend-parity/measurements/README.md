# backend-parity-c verify measurements

Durable evidence behind the `ci` flags in `tests/e2e/manifests/backend-parity-c.toml`.
A flag flip in that manifest should cite a row here.

## `verify-sweep-6e1b59af0.tsv`

Every `(test, backend)` pair that `[test.modes.verify]` declares in `backends_enabled`,
measured twice.

| | |
|---|---|
| Hermit SHA | `6e1b59af0b228403fb3e9bcafc63da64a531569f` (debug build) |
| Reverie | unchanged (no pin moved by this branch) |
| Host | devbig014 |
| Backends | as declared per cell; ptrace for 72 of 85 tests, plus dbi/kvm/sabre/liteinst on 13 |
| Date | 2026-08-06 |

Result: **108 cells, 85 PASS twice, 23 FAIL twice, 0 disagreements between the two sweeps.**
**ptrace verify passes on all 85 of 85 tests.** Every JUnit file carried `tests >= 1`, so no
row is a zero-test no-result.

### Method

One harness invocation per cell:

```
LD_LIBRARY_PATH=<libunwind> \
E2E_RUN_ID=<unique-per-cell> \
./ci/test_harness.sh run --lane <lane> --test <id> --mode verify --include-manual \
    --results <per-cell path> --junit <per-cell path>
```

Sweep 1 ran at parallelism 6 and sweep 2 at parallelism 4 — deliberately different
concurrency, so a contention artifact would surface as a disagreement between the two
columns rather than hide in both.

### Traps that have cost previous sweeps real time

- **A cell can emit more than one `verify` record — one per enabled backend.** 13 of the
  85 tests declare more than ptrace. Aggregate per `(test, backend)`; an aggregator that
  assumes one record per test misreads those 13.
- **`ci` is per mode-cell, not per backend.** Setting `ci = true` runs *every* backend in
  `backends_enabled`. A cell whose ptrace column is green can still be unenableable.
- **`backend-parity-c/cpuid-probe` is the only `lane = "privileged"` test.** A
  `--lane portable` sweep drops it and the harness exits 2 with
  `filters selected no required test cells`, which reads like a failure and is not.
- **A shared `--results` path is truncated per invocation.** Use per-cell files.
- **Do not edit the tree while a sweep runs.** Editing `ci/manifest-plan` mid-sweep once
  made `manifest-plan` die on every cell, so the harness's test list came back empty and
  every later invocation exited 2 — void results that look like data.
- **Read the verdict from the `outcome` field of the per-cell JSONL, filtered to
  `mode == "verify"`** — not by grepping the log for the first `PASS|FAIL|ERROR` token.
  A previous sweep got the right count and the wrong set that way.
