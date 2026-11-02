# Compatibility scorecard

This table is derived from the manifest, not from a separately maintained parent-workspace CSV. `./ci/compat-envelope/scorecard.rs check` verifies it.

The count table includes all **11571** cells in the manifest; no row is omitted. A cell is **Selected by full** exactly when it appears in `ci/expected-e2e-plan.json`. A cell is **Not selected by full** when it is in the manifest but absent from that plan. Selection is not a test result: a cell not selected by full may have passed, failed, produced no verdict, or never run. Of these cells, **2593** are selected by full, **523** are not selected by full, and **8455** are **Not applicable**.

Every selected `verify` cell that does not declare the stripped comparator, and every seed in a selected `chaos` cell, runs the same backend twice. The manifest runner adds `--verify-strict` when the selected Hermit binary supports it, and accepts a result only when the typed report says `verified=true`, `verdict=matched`, `bitwise_parity=true`, `strictness=canonical`, `compare_logs=true`, a named canonical `record_envelope`, and both INFO-message counts are nonzero. Bare `--verify` remains a Stripped comparison when invoked directly and does not satisfy this regression plan. None of the **2338** selected `verify` cells declares the stripped comparator. These same-backend results do not establish cross-backend parity.

| Backend | Selected by full | Not selected by full | Not applicable | In the manifest |
| --- | ---: | ---: | ---: | ---: |
| `ptrace` | 851 | 12 | 964 | 1827 |
| `dbt` | 549 | 1 | 1277 | 1827 |
| `kvm` | 570 | 0 | 1257 | 1827 |
| `sabre` | 0 | 477 | 1350 | 1827 |
| `liteinst` | 312 | 0 | 1515 | 1827 |
| `in-guest-trap` | 311 | 0 | 1516 | 1827 |
| `native` | 0 | 33 | 576 | 609 |
| **Total** | **2593** | **523** | **8455** | **11571** |

**Archived:** `sabre` (backend, archived 2026-10-25, nightly only) and `e9patch` (preprocessing with ptrace, not a backend, archived 2026-10-25, nightly only). They get no feature work and their tests left per-commit validation. An archived backend's cells that would be selected by full belong to the `archived-nightly` run type instead, so the table counts them as not selected by full, and the nightly pressure sweep runs them as red cells. `ci/manifest-plan/src/archived.rs` records what still runs per commit.

## Denominator, and why the percentage is not comparable across changes to it

Selected by full is **2593 of 11571**, which is **22.41%** — over THIS population and no other. The population is every combination the manifest declares, and it is composed of:

- backends: `ptrace`, `dbt`, `kvm`, `sabre`, `liteinst`, `in-guest-trap`, `native`
- modes: `chaos`, `naked`, `replay`, `verify`

⚠️ **8455 of those 11571 cells are NOT APPLICABLE** — their backend is not applicable for their mode, so they were never asked to run and cannot pass or fail. Over the 3116 cells that CAN run, selected by full is **83.22%**.

⚠️ **DO NOT QUOTE THAT SECOND FIGURE AS PROGRESS.** It is the same 2593 cells selected by full measured against a smaller denominator. Nothing was fixed to produce it; it is what the first figure always meant once the cells that cannot run are excluded. Quote both or neither, and never compare one against the other as though something moved.

⚠️ **Adding or removing a backend or mode changes this denominator and therefore the percentage, without anything about the product changing.** Removing a backend whose cells are mostly not selected RAISES the reported figure; adding manifest cells that are not selected LOWERS it. Neither is progress. Before comparing this percentage against an earlier one, diff the two lists above: if they differ, the numbers are not comparable and the difference is not a result.

The mode view makes the current order of work explicit: expand `verify` first, then `replay`, then `chaos`. Each backend cell is `selected by full / in the manifest`; an em dash means that mode does not exist for that backend. The summary columns use the same selection and applicability facts as the table above.

| Mode | `ptrace` | `dbt` | `kvm` | `sabre` | `liteinst` | `in-guest-trap` | `native` | Selected by full | Not selected by full | Not applicable | In the manifest |
| --- | ---: | ---: | ---: | ---: | ---: | ---: | ---: | ---: | ---: | ---: | ---: |
| `verify` | 596 / 609 | 549 / 609 | 570 / 609 | 0 / 609 | 312 / 609 | 311 / 609 | — | 2338 | 487 | 829 | 3654 |
| `replay` | 248 / 609 | 0 / 609 | 0 / 609 | 0 / 609 | 0 / 609 | 0 / 609 | — | 248 | 3 | 3403 | 3654 |
| `chaos` | 7 / 609 | 0 / 609 | 0 / 609 | 0 / 609 | 0 / 609 | 0 / 609 | — | 7 | 0 | 3647 | 3654 |
| `naked` | — | — | — | — | — | — | 0 / 609 | 0 | 33 | 576 | 609 |
| **Total** | | | | | | | | **2593** | **523** | **8455** | **11571** |

## Ptrace by manifest category

This view uses the same Basic Sanity Milestone 1 contracts as the tables above, but makes the ptrace workload mix visible. Each entry is `selected by full / in the manifest`; `custom` commands are not part of this denominator.

| Manifest category | Verify | Replay | Chaos | Selected by full | In the manifest |
| --- | ---: | ---: | ---: | ---: | ---: |
| `applications` | 4 / 6 | 0 / 6 | 0 / 6 | 4 | 18 |
| `bin-c` | 2 / 2 | 0 / 2 | 0 / 2 | 2 | 6 |
| `c-programs` | 291 / 293 | 35 / 293 | 3 / 293 | 329 | 879 |
| `chaos-c` | 1 / 1 | 0 / 1 | 1 / 1 | 2 | 3 |
| `compat` | 214 / 219 | 212 / 219 | 0 / 219 | 426 | 657 |
| `data-handling` | 6 / 6 | 0 / 6 | 0 / 6 | 6 | 18 |
| `debugger-c` | 1 / 1 | 0 / 1 | 0 / 1 | 1 | 3 |
| `determinism-stress` | 5 / 6 | 0 / 6 | 2 / 6 | 7 | 18 |
| `determinism-stress-c` | 11 / 11 | 0 / 11 | 1 / 11 | 12 | 33 |
| `language-runtimes` | 19 / 19 | 0 / 19 | 0 / 19 | 19 | 57 |
| `shared-futex-c` | 1 / 4 | 0 / 4 | 0 / 4 | 1 | 12 |
| `system-utils` | 40 / 40 | 1 / 40 | 0 / 40 | 41 | 120 |
| `util-c` | 1 / 1 | 0 / 1 | 0 / 1 | 1 | 3 |

Ordinary full validation executes 2600 cells: the 2593 comparable compatibility cells selected by full above (including 7 chaos-mode race-exposure checks), and 7 explicit custom commands outside the comparable denominator. A passing validate must produce a fresh result for all of them; a failing selected cell is a regression, not permission to remove it from the plan.

### Selected custom commands outside the comparable denominator

These rows are part of the selected regression denominator even though they are not rows in `ci/compat-envelope/cells.json`. Their exact identities come from `ci/expected-e2e-plan.json`; `scorecard.rs check` refuses any selected row that is not accounted for by either this table or the comparable green cells above.

| Lane | Category | Test | Mode | Backend |
| --- | --- | --- | --- | --- |
| `portable` | `c-programs` | `c-programs/environment-and-workdir` | `custom` | `ptrace` |
| `portable` | `c-programs` | `c-programs/inotify-protection-key-fault` | `custom` | `dbt` |
| `portable` | `c-programs` | `c-programs/inotify-protection-key-fault` | `custom` | `ptrace` |
| `portable` | `c-programs` | `c-programs/io-uring-fallback` | `custom` | `dbt` |
| `portable` | `c-programs` | `c-programs/io-uring-fallback` | `custom` | `ptrace` |
| `portable` | `c-programs` | `c-programs/random-device-spellings` | `custom` | `ptrace` |
| `portable` | `system-utils` | `system-utils/clock-determinism` | `custom` | `ptrace` |

## Run history

Detailed observations and the generated history website live in [hermit_test_ledger](https://github.com/rrnewton/hermit_test_ledger). This catalogue records selection and applicability, not whether a cell has been measured. Run `./ci/compat-envelope/scorecard.rs show` with the ledger checkout available to read history.
