# Compatibility scorecard

This table is derived from the manifest, not from a separately maintained parent-workspace CSV. `./ci/compat-envelope/scorecard.rs check` verifies it.

**Green** means the cell is in `ci/expected-e2e-plan.json`, is not a chaos-mode race-exposure check, and is therefore required to pass by ordinary validation. **Red** is every other `hermit-manifest-plan` cell: measured failure, unavailable, or not yet run all remain red until the cell is promoted into the regression plan and passes. Combinations listed under `backends_disabled` are outside this runnable denominator.

These are the current pre-basic-sanity contracts. In particular, bare `--verify` uses the Stripped comparator and this table does not relabel it as strict INFO-log parity.

| Backend | Green | Red | Total |
| --- | ---: | ---: | ---: |
| `ptrace` | 149 | 188 | 337 |
| `dbt` | 9 | 53 | 62 |
| `kvm` | 0 | 23 | 23 |
| `sabre` | 9 | 132 | 141 |
| `liteinst` | 3 | 28 | 31 |
| `native` | 0 | 33 | 33 |
| **Total** | **170** | **457** | **627** |

Ordinary full validation executes 172 selected regression cells: the 170 green compatibility cells above plus 2 chaos-mode race-exposure checks. A passing validate must produce a fresh result for all of them; a failing green cell is a regression, not permission to move it to red.
