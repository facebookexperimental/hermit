# Compatibility scorecard

This table is derived from the manifest, not from a separately maintained parent-workspace CSV. `./ci/compat-envelope/scorecard.rs check` verifies it.

The count table includes all **11761** cells in the manifest; no row is omitted. A cell is **Selected by full** exactly when it appears in `ci/expected-e2e-plan.json`. A cell is **Not selected by full** when it is in the manifest but absent from that plan. Selection is not a test result: a cell not selected by full may have passed, failed, produced no verdict, or never run. Of these cells, **2394** are selected by full, **1095** are not selected by full, and **8272** are **Not applicable**.

Every selected `verify` cell that does not declare the stripped comparator, and every seed in a selected `chaos` cell, runs the same backend twice. The manifest runner adds `--verify-strict` when the selected Hermit binary supports it, and accepts a result only when the typed report says `verified=true`, `verdict=matched`, `bitwise_parity=true`, `strictness=canonical`, `compare_logs=true`, a named canonical `record_envelope`, and both INFO-message counts are nonzero. Bare `--verify` remains a Stripped comparison when invoked directly and does not satisfy this regression plan. None of the **1824** selected `verify` cells declares the stripped comparator. These same-backend results do not establish cross-backend parity.

| Backend | Selected by full | Not selected by full | Not applicable | In the manifest |
| --- | ---: | ---: | ---: | ---: |
| `ptrace` | 1176 | 18 | 663 | 1857 |
| `dbt` | 0 | 561 | 1296 | 1857 |
| `kvm` | 587 | 5 | 1265 | 1857 |
| `sabre` | 0 | 478 | 1379 | 1857 |
| `liteinst` | 316 | 0 | 1541 | 1857 |
| `in-guest-trap` | 315 | 0 | 1542 | 1857 |
| `native` | 0 | 33 | 586 | 619 |
| **Total** | **2394** | **1095** | **8272** | **11761** |

**Archived:** `sabre` (backend, archived 2026-10-25, nightly only) and `e9patch` (preprocessing with ptrace, not a backend, archived 2026-10-25, nightly only). They get no feature work and their tests left per-commit validation. An archived backend's cells that would be selected by full belong to the `archived-nightly` run type instead, so the table counts them as not selected by full, and the nightly pressure sweep runs them as red cells. `ci/manifest-plan/src/archived.rs` records what still runs per commit.

## Denominator, and why the percentage is not comparable across changes to it

Selected by full is **2394 of 11761**, which is **20.36%** — over THIS population and no other. The population is every combination the manifest declares, and it is composed of:

- backends: `ptrace`, `dbt`, `kvm`, `sabre`, `liteinst`, `in-guest-trap`, `native`
- modes: `chaos`, `naked`, `replay`, `verify`

⚠️ **8272 of those 11761 cells are NOT APPLICABLE** — their backend is not applicable for their mode, so they were never asked to run and cannot pass or fail. Over the 3489 cells that CAN run, selected by full is **68.62%**.

⚠️ **DO NOT QUOTE THAT SECOND FIGURE AS PROGRESS.** It is the same 2394 cells selected by full measured against a smaller denominator. Nothing was fixed to produce it; it is what the first figure always meant once the cells that cannot run are excluded. Quote both or neither, and never compare one against the other as though something moved.

⚠️ **Adding or removing a backend or mode changes this denominator and therefore the percentage, without anything about the product changing.** Removing a backend whose cells are mostly not selected RAISES the reported figure; adding manifest cells that are not selected LOWERS it. Neither is progress. Before comparing this percentage against an earlier one, diff the two lists above: if they differ, the numbers are not comparable and the difference is not a result.

The mode view makes the current order of work explicit: expand `verify` first, then `replay`, then `chaos`. Each backend cell is `selected by full / in the manifest`; an em dash means that mode does not exist for that backend. The summary columns use the same selection and applicability facts as the table above.

| Mode | `ptrace` | `dbt` | `kvm` | `sabre` | `liteinst` | `in-guest-trap` | `native` | Selected by full | Not selected by full | Not applicable | In the manifest |
| --- | ---: | ---: | ---: | ---: | ---: | ---: | ---: | ---: | ---: | ---: | ---: |
| `verify` | 606 / 619 | 0 / 619 | 587 / 619 | 0 / 619 | 316 / 619 | 315 / 619 | — | 1824 | 1056 | 834 | 3714 |
| `replay` | 563 / 619 | 0 / 619 | 0 / 619 | 0 / 619 | 0 / 619 | 0 / 619 | — | 563 | 6 | 3145 | 3714 |
| `chaos` | 7 / 619 | 0 / 619 | 0 / 619 | 0 / 619 | 0 / 619 | 0 / 619 | — | 7 | 0 | 3707 | 3714 |
| `naked` | — | — | — | — | — | — | 0 / 619 | 0 | 33 | 586 | 619 |
| **Total** | | | | | | | | **2394** | **1095** | **8272** | **11761** |

## Ptrace by manifest category

This view uses the same Basic Sanity Milestone 1 contracts as the tables above, but makes the ptrace workload mix visible. Each entry is `selected by full / in the manifest`; `custom` commands are not part of this denominator.

| Manifest category | Verify | Replay | Chaos | Selected by full | In the manifest |
| --- | ---: | ---: | ---: | ---: | ---: |
| `applications` | 7 / 7 | 6 / 7 | 0 / 7 | 13 | 21 |
| `bin-c` | 2 / 2 | 1 / 2 | 0 / 2 | 3 | 6 |
| `c-programs` | 298 / 300 | 261 / 300 | 3 / 300 | 562 | 900 |
| `chaos-c` | 1 / 1 | 1 / 1 | 1 / 1 | 3 | 3 |
| `compat` | 212 / 219 | 212 / 219 | 0 / 219 | 424 | 657 |
| `data-handling` | 6 / 6 | 6 / 6 | 0 / 6 | 12 | 18 |
| `debugger-c` | 1 / 1 | 1 / 1 | 0 / 1 | 2 | 3 |
| `determinism-stress` | 6 / 6 | 6 / 6 | 2 / 6 | 14 | 18 |
| `determinism-stress-c` | 11 / 11 | 11 / 11 | 1 / 11 | 23 | 33 |
| `language-runtimes` | 20 / 21 | 19 / 21 | 0 / 21 | 39 | 63 |
| `shared-futex-c` | 1 / 4 | 0 / 4 | 0 / 4 | 1 | 12 |
| `system-utils` | 40 / 40 | 39 / 40 | 0 / 40 | 79 | 120 |
| `util-c` | 1 / 1 | 0 / 1 | 0 / 1 | 1 | 3 |

Ordinary full validation executes 2399 cells: the 2394 comparable compatibility cells selected by full above (including 7 chaos-mode race-exposure checks), and 5 explicit custom commands outside the comparable denominator. A passing validate must produce a fresh result for all of them; a failing selected cell is a regression, not permission to remove it from the plan.

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

Each row below is a test that a validation node runs and expects to FAIL on its backend: a known gap, listed with its tracking issue in the node's known-gap ledger. These **6** rows are not manifest cells, so they are in none of the counts or percentages above. Each one counts as a failing test, never as a pass and never as not applicable. The node records it as `diagnostic_fail` when it fails the way its row says, and fails outright when it passes (promote it to required) or fails some other way. In a signature, `\n` is a newline and `<N>` any run of digits. The same rows, with their tracking URLs, are in `ci/compat-envelope/known-gaps.json`.

### `privileged-test.deadline_slice_cap_kvm` (`--backend=kvm`)

**36** required tests must pass; **6** known gaps, from `hermit-cli/tests/deadline_slice_cap.kvm-gaps.tsv`:

| Test | Gap | Tracking issue | Must fail with | Status |
| --- | --- | --- | --- | --- |
| `itimer_a_private_descriptor_table_consumer_re_arms` | D | https://github.com/rrnewton/hermit/issues/4929 | output showing `"privfd" "5000"\nstatus: exit status: 122\nstdout:\n\nstderr:` | failing (known gap) |
| `itimer_a_sigsuspend_loop_takes_every_expiry` | C | https://github.com/rrnewton/hermit/issues/4929 | output showing `failed: deliveries=` | failing (known gap) |
| `itimer_a_wait_mask_is_withdrawn_when_the_wait_returns` | F' | https://github.com/rrnewton/hermit/issues/4929 | output showing `"stale" "ppoll_mask"\nstatus: exit status: 122` | failing (known gap) |
| `itimer_an_ignored_expiry_stops_the_timer_until_a_dequeue` | E | https://github.com/rrnewton/hermit/issues/4929 | output showing `failed: ignored_remaining_us=` | failing (known gap) |
| `itimer_generation_discard_follows_the_wait_mask` | E+F' | https://github.com/rrnewton/hermit/issues/4929 | output showing `failed: poll: result=0 first_us=` | failing (known gap) |
| `itimer_mixed_masks_keep_the_timer_running` | G | https://github.com/rrnewton/hermit/issues/4929 | output showing `\npositive=<N> samples=20 progress=<N>\n\nnote: run with` | failing (known gap) |

## Run history

Detailed observations and the generated history website live in [hermit_test_ledger](https://github.com/rrnewton/hermit_test_ledger). This catalogue records selection and applicability, not whether a cell has been measured. Run `./ci/compat-envelope/scorecard.rs show` with the ledger checkout available to read history.
