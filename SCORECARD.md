# Compatibility scorecard

This table is derived from the manifest, not from a separately maintained parent-workspace CSV. `./ci/compat-envelope/scorecard.rs check` verifies it.

The count table includes all **17803** cells in the manifest; no row is omitted. A cell is **Selected by full** exactly when it appears in `ci/expected-e2e-plan.json`. A cell is **Not selected by full** when it is in the manifest but absent from that plan. Selection is not a test result: a cell not selected by full may have passed, failed, produced no verdict, or never run. Of these cells, **2434** are selected by full, **1845** are not selected by full, and **13524** are **Not applicable**.

Every selected `verify` cell that does not declare the stripped comparator, and every seed in a selected `chaos` cell, runs the same backend twice. The manifest runner adds `--verify-strict` when the selected Hermit binary supports it, and accepts a result only when the typed report says `verified=true`, `verdict=matched`, `bitwise_parity=true`, `strictness=canonical`, `compare_logs=true`, a named canonical `record_envelope`, and both INFO-message counts are nonzero. Bare `--verify` remains a Stripped comparison when invoked directly and does not satisfy this regression plan. None of the **1855** selected `verify` cells declares the stripped comparator. These same-backend results do not establish cross-backend parity.

| Backend | Selected by full | Not selected by full | Not applicable | In the manifest |
| --- | ---: | ---: | ---: | ---: |
| `ptrace` | 1193 | 359 | 1259 | 2811 |
| `dbt` | 0 | 561 | 2250 | 2811 |
| `kvm` | 610 | 140 | 2061 | 2811 |
| `sabre` | 0 | 480 | 2331 | 2811 |
| `liteinst` | 316 | 136 | 2359 | 2811 |
| `in-guest-trap` | 315 | 136 | 2360 | 2811 |
| `native` | 0 | 33 | 904 | 937 |
| **Total** | **2434** | **1845** | **13524** | **17803** |

**Archived:** `sabre` (backend, archived 2026-10-25, nightly only) and `e9patch` (preprocessing with ptrace, not a backend, archived 2026-10-25, nightly only). They get no feature work and their tests left per-commit validation. An archived backend's cells that would be selected by full belong to the `archived-nightly` run type instead, so the table counts them as not selected by full, and the nightly pressure sweep runs them as red cells. `ci/manifest-plan/src/archived.rs` records what still runs per commit.

## Denominator, and why the percentage is not comparable across changes to it

Selected by full is **2434 of 17803**, which is **13.67%** — over THIS population and no other. The population is every combination the manifest declares, and it is composed of:

- backends: `ptrace`, `dbt`, `kvm`, `sabre`, `liteinst`, `in-guest-trap`, `native`
- modes: `chaos`, `naked`, `replay`, `verify`

⚠️ **13524 of those 17803 cells are NOT APPLICABLE** — their backend is not applicable for their mode, so they were never asked to run and cannot pass or fail. Over the 4279 cells that CAN run, selected by full is **56.88%**.

⚠️ **DO NOT QUOTE THAT SECOND FIGURE AS PROGRESS.** It is the same 2434 cells selected by full measured against a smaller denominator. Nothing was fixed to produce it; it is what the first figure always meant once the cells that cannot run are excluded. Quote both or neither, and never compare one against the other as though something moved.

⚠️ **Adding or removing a backend or mode changes this denominator and therefore the percentage, without anything about the product changing.** Removing a backend whose cells are mostly not selected RAISES the reported figure; adding manifest cells that are not selected LOWERS it. Neither is progress. Before comparing this percentage against an earlier one, diff the two lists above: if they differ, the numbers are not comparable and the difference is not a result.

The mode view makes the current order of work explicit: expand `verify` first, then `replay`, then `chaos`. Each backend cell is `selected by full / in the manifest`; an em dash means that mode does not exist for that backend. The summary columns use the same selection and applicability facts as the table above.

| Mode | `ptrace` | `dbt` | `kvm` | `sabre` | `liteinst` | `in-guest-trap` | `native` | Selected by full | Not selected by full | Not applicable | In the manifest |
| --- | ---: | ---: | ---: | ---: | ---: | ---: | ---: | ---: | ---: | ---: | ---: |
| `verify` | 614 / 937 | 0 / 937 | 610 / 937 | 0 / 937 | 316 / 937 | 315 / 937 | — | 1855 | 1775 | 1992 | 5622 |
| `replay` | 572 / 937 | 0 / 937 | 0 / 937 | 0 / 937 | 0 / 937 | 0 / 937 | — | 572 | 37 | 5013 | 5622 |
| `chaos` | 7 / 937 | 0 / 937 | 0 / 937 | 0 / 937 | 0 / 937 | 0 / 937 | — | 7 | 0 | 5615 | 5622 |
| `naked` | — | — | — | — | — | — | 0 / 937 | 0 | 33 | 904 | 937 |
| **Total** | | | | | | | | **2434** | **1845** | **13524** | **17803** |

## Ptrace by manifest category

This view uses the same Basic Sanity Milestone 1 contracts as the tables above, but makes the ptrace workload mix visible. Each entry is `selected by full / in the manifest`; `custom` commands are not part of this denominator.

| Manifest category | Verify | Replay | Chaos | Selected by full | In the manifest |
| --- | ---: | ---: | ---: | ---: | ---: |
| `applications` | 7 / 7 | 7 / 7 | 0 / 7 | 14 | 21 |
| `bin-c` | 2 / 2 | 1 / 2 | 0 / 2 | 3 | 6 |
| `c-programs` | 303 / 305 | 265 / 305 | 3 / 305 | 571 | 915 |
| `chaos-c` | 1 / 1 | 1 / 1 | 1 / 1 | 3 | 3 |
| `compat` | 212 / 221 | 215 / 221 | 0 / 221 | 427 | 663 |
| `data-handling` | 6 / 6 | 6 / 6 | 0 / 6 | 12 | 18 |
| `debugger-c` | 1 / 1 | 1 / 1 | 0 / 1 | 2 | 3 |
| `determinism-stress` | 6 / 6 | 6 / 6 | 2 / 6 | 14 | 18 |
| `determinism-stress-c` | 11 / 11 | 11 / 11 | 1 / 11 | 23 | 33 |
| `gvisor` | 0 / 277 | 0 / 277 | 0 / 277 | 0 | 831 |
| `language-runtimes` | 22 / 22 | 19 / 22 | 0 / 22 | 41 | 66 |
| `rr` | 0 / 32 | 0 / 32 | 0 / 32 | 0 | 96 |
| `shared-futex-c` | 1 / 4 | 0 / 4 | 0 / 4 | 1 | 12 |
| `system-utils` | 41 / 41 | 40 / 41 | 0 / 41 | 81 | 123 |
| `util-c` | 1 / 1 | 0 / 1 | 0 / 1 | 1 | 3 |

Ordinary full validation executes 2439 cells: the 2434 comparable compatibility cells selected by full above (including 7 chaos-mode race-exposure checks), and 5 explicit custom commands outside the comparable denominator. A passing validate must produce a fresh result for all of them; a failing selected cell is a regression, not permission to remove it from the plan.

### Selected custom commands outside the comparable denominator

These rows are part of the selected regression denominator even though they are not rows in `ci/compat-envelope/cells.json`. Their exact identities come from `ci/expected-e2e-plan.json`; `scorecard.rs check` refuses any selected row that is not accounted for by either this table or the comparable green cells above.

| Lane | Category | Test | Mode | Backend |
| --- | --- | --- | --- | --- |
| `portable` | `c-programs` | `c-programs/environment-and-workdir` | `custom` | `ptrace` |
| `portable` | `c-programs` | `c-programs/inotify-protection-key-fault` | `custom` | `ptrace` |
| `portable` | `c-programs` | `c-programs/io-uring-fallback` | `custom` | `ptrace` |
| `portable` | `c-programs` | `c-programs/random-device-spellings` | `custom` | `ptrace` |
| `portable` | `system-utils` | `system-utils/clock-determinism` | `custom` | `ptrace` |

## Known gaps of validation nodes, outside every count above

Each row below is a test that a validation node does not yet require on its backend: a known gap, listed with its tracking issue in the node's known-gap ledger. The node runs a known gap and expects it to FAIL, or, when its row says skipped, leaves it out of the run by name. These **2** rows are not manifest cells, so they are in none of the counts or percentages above. Each one counts as a failing test, never as a pass and never as not applicable. The node records a known gap it runs as `diagnostic_fail` when it fails the way its row says, and fails outright when it passes (promote it to required) or fails some other way; a skipped one is not run, so nothing shows whether it still fails until its row is deleted. In a signature, `\n` is a newline and `<N>` any run of digits. The same rows, with their tracking URLs, are in `ci/compat-envelope/known-gaps.json`.

### `privileged-test.deadline_slice_cap_kvm` (`--backend=kvm`)

**43** required tests must pass; **2** known gaps (2 skipped, not run), from `hermit-cli/tests/deadline_slice_cap.kvm-gaps.tsv`:

| Test | Gap | Tracking issue | Must fail with | Status |
| --- | --- | --- | --- | --- |
| `itimer_a_private_descriptor_table_consumer_re_arms` | D | https://github.com/rrnewton/hermit/issues/4485 | not run | skipped (known gap) |
| `itimer_generation_discard_follows_the_wait_mask` | E+F' | https://github.com/rrnewton/hermit/issues/4995 | not run | skipped (known gap) |

## Run history

Detailed observations and the generated history website live in [hermit_test_ledger](https://github.com/rrnewton/hermit_test_ledger). This catalogue records selection and applicability, not whether a cell has been measured. Run `./ci/compat-envelope/scorecard.rs show` with the ledger checkout available to read history.
