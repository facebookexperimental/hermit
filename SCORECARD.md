# Compatibility scorecard

This table is derived from the manifest, not from a separately maintained parent-workspace CSV. `./ci/compat-envelope/scorecard.rs check` verifies it.

The count table includes all **11248** cells in the manifest; no row is omitted. A cell is **Selected by full** exactly when it appears in `ci/expected-e2e-plan.json`. A cell is **Not selected by full** when it is in the manifest but absent from that plan. Selection is not a test result: a cell not selected by full may have passed, failed, produced no verdict, or never run. Of these cells, **2478** are selected by full, **280** are not selected by full, and **8490** are **Not applicable**.

Every selected `verify` cell that does not declare the stripped comparator, and every seed in a selected `chaos` cell, runs the same backend twice. The manifest runner adds `--verify-strict` when the selected Hermit binary supports it, and accepts a result only when the typed report says `verified=true`, `verdict=matched`, `bitwise_parity=true`, `strictness=canonical`, `compare_logs=true`, a named canonical `record_envelope`, and both INFO-message counts are nonzero. Bare `--verify` remains a Stripped comparison when invoked directly and does not satisfy this regression plan. None of the **2223** selected `verify` cells declares the stripped comparator. These same-backend results do not establish cross-backend parity.

| Backend | Selected by full | Not selected by full | Not applicable | In the manifest |
| --- | ---: | ---: | ---: | ---: |
| `ptrace` | 834 | 12 | 930 | 1776 |
| `dbt` | 249 | 1 | 1526 | 1776 |
| `kvm` | 552 | 1 | 1223 | 1776 |
| `sabre` | 236 | 233 | 1307 | 1776 |
| `liteinst` | 304 | 0 | 1472 | 1776 |
| `in-guest-trap` | 303 | 0 | 1473 | 1776 |
| `native` | 0 | 33 | 559 | 592 |
| **Total** | **2478** | **280** | **8490** | **11248** |

## Denominator, and why the percentage is not comparable across changes to it

Selected by full is **2478 of 11248**, which is **22.03%** — over THIS population and no other. The population is every combination the manifest declares, and it is composed of:

- backends: `ptrace`, `dbt`, `kvm`, `sabre`, `liteinst`, `in-guest-trap`, `native`
- modes: `chaos`, `naked`, `replay`, `verify`

⚠️ **8490 of those 11248 cells are NOT APPLICABLE** — their backend is not applicable for their mode, so they were never asked to run and cannot pass or fail. Over the 2758 cells that CAN run, selected by full is **89.85%**.

⚠️ **DO NOT QUOTE THAT SECOND FIGURE AS PROGRESS.** It is the same 2478 cells selected by full measured against a smaller denominator. Nothing was fixed to produce it; it is what the first figure always meant once the cells that cannot run are excluded. Quote both or neither, and never compare one against the other as though something moved.

⚠️ **Adding or removing a backend or mode changes this denominator and therefore the percentage, without anything about the product changing.** Removing a backend whose cells are mostly not selected RAISES the reported figure; adding manifest cells that are not selected LOWERS it. Neither is progress. Before comparing this percentage against an earlier one, diff the two lists above: if they differ, the numbers are not comparable and the difference is not a result.

The mode view makes the current order of work explicit: expand `verify` first, then `replay`, then `chaos`. Each backend cell is `selected by full / in the manifest`; an em dash means that mode does not exist for that backend. The summary columns use the same selection and applicability facts as the table above.

| Mode | `ptrace` | `dbt` | `kvm` | `sabre` | `liteinst` | `in-guest-trap` | `native` | Selected by full | Not selected by full | Not applicable | In the manifest |
| --- | ---: | ---: | ---: | ---: | ---: | ---: | ---: | ---: | ---: | ---: | ---: |
| `verify` | 579 / 592 | 249 / 592 | 552 / 592 | 236 / 592 | 304 / 592 | 303 / 592 | — | 2223 | 244 | 1085 | 3552 |
| `replay` | 248 / 592 | 0 / 592 | 0 / 592 | 0 / 592 | 0 / 592 | 0 / 592 | — | 248 | 3 | 3301 | 3552 |
| `chaos` | 7 / 592 | 0 / 592 | 0 / 592 | 0 / 592 | 0 / 592 | 0 / 592 | — | 7 | 0 | 3545 | 3552 |
| `naked` | — | — | — | — | — | — | 0 / 592 | 0 | 33 | 559 | 592 |
| **Total** | | | | | | | | **2478** | **280** | **8490** | **11248** |

## Ptrace by manifest category

This view uses the same Basic Sanity Milestone 1 contracts as the tables above, but makes the ptrace workload mix visible. Each entry is `selected by full / in the manifest`; `custom` commands are not part of this denominator.

| Manifest category | Verify | Replay | Chaos | Selected by full | In the manifest |
| --- | ---: | ---: | ---: | ---: | ---: |
| `applications` | 4 / 6 | 0 / 6 | 0 / 6 | 4 | 18 |
| `bin-c` | 2 / 2 | 0 / 2 | 0 / 2 | 2 | 6 |
| `c-programs` | 276 / 278 | 35 / 278 | 3 / 278 | 314 | 834 |
| `chaos-c` | 1 / 1 | 0 / 1 | 1 / 1 | 2 | 3 |
| `compat` | 214 / 219 | 212 / 219 | 0 / 219 | 426 | 657 |
| `data-handling` | 6 / 6 | 0 / 6 | 0 / 6 | 6 | 18 |
| `debugger-c` | 1 / 1 | 0 / 1 | 0 / 1 | 1 | 3 |
| `determinism-stress` | 5 / 6 | 0 / 6 | 2 / 6 | 7 | 18 |
| `determinism-stress-c` | 11 / 11 | 0 / 11 | 1 / 11 | 12 | 33 |
| `language-runtimes` | 19 / 19 | 0 / 19 | 0 / 19 | 19 | 57 |
| `shared-futex-c` | 1 / 4 | 0 / 4 | 0 / 4 | 1 | 12 |
| `system-utils` | 38 / 38 | 1 / 38 | 0 / 38 | 39 | 114 |
| `util-c` | 1 / 1 | 0 / 1 | 0 / 1 | 1 | 3 |

Ordinary full validation executes 2483 cells: the 2478 comparable compatibility cells selected by full above (including 7 chaos-mode race-exposure checks), and 5 explicit custom commands outside the comparable denominator. A passing validate must produce a fresh result for all of them; a failing selected cell is a regression, not permission to remove it from the plan.

### Selected custom commands outside the comparable denominator

These rows are part of the selected regression denominator even though they are not rows in `ci/compat-envelope/cells.json`. Their exact identities come from `ci/expected-e2e-plan.json`; `scorecard.rs check` refuses any selected row that is not accounted for by either this table or the comparable green cells above.

| Lane | Category | Test | Mode | Backend |
| --- | --- | --- | --- | --- |
| `portable` | `c-programs` | `c-programs/environment-and-workdir` | `custom` | `ptrace` |
| `portable` | `c-programs` | `c-programs/io-uring-fallback` | `custom` | `dbt` |
| `portable` | `c-programs` | `c-programs/io-uring-fallback` | `custom` | `ptrace` |
| `portable` | `c-programs` | `c-programs/random-device-spellings` | `custom` | `ptrace` |
| `portable` | `system-utils` | `system-utils/clock-determinism` | `custom` | `ptrace` |

## Run history

Detailed observations and the generated history website live in [hermit_test_ledger](https://github.com/rrnewton/hermit_test_ledger). This catalogue records selection and applicability, not whether a cell has been measured. Run `./ci/compat-envelope/scorecard.rs show` with the ledger checkout available to read history.
