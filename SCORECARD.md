# Compatibility scorecard

This table is derived from the manifest, not from a separately maintained parent-workspace CSV. `./ci/compat-envelope/scorecard.rs check` verifies it.

**Green** means the cell is in `ci/expected-e2e-plan.json`, is not a chaos-mode race-exposure check, and is therefore required to pass by ordinary validation. **Red** is every other test/mode/backend cell: measured failure, unavailable, or not yet run all remain red until the cell is promoted into the regression plan and passes. Manifest-disabled combinations are red, not omitted: a cell that cannot run is not green.

These are the current pre-basic-sanity contracts. In particular, bare `--verify` uses the Stripped comparator and this table does not relabel it as strict INFO-log parity.

| Backend | Green | Red | Total |
| --- | ---: | ---: | ---: |
| `ptrace` | 149 | 1195 | 1344 |
| `dbt` | 9 | 1335 | 1344 |
| `kvm` | 0 | 1344 | 1344 |
| `sabre` | 9 | 1335 | 1344 |
| `liteinst` | 3 | 1341 | 1344 |
| `native` | 0 | 336 | 336 |
| **Total** | **170** | **6886** | **7056** |

The mode view makes the current order of work explicit: expand `verify` first, then `replay`, then `chaos`. Each backend cell is `green / total`; an em dash means that mode does not exist for that backend.

| Mode | `ptrace` | `dbt` | `kvm` | `sabre` | `liteinst` | `native` | Green | Red | Total |
| --- | ---: | ---: | ---: | ---: | ---: | ---: | ---: | ---: | ---: |
| `verify` | 147 / 336 | 9 / 336 | 0 / 336 | 9 / 336 | 2 / 336 | — | 167 | 1513 | 1680 |
| `replay` | 1 / 336 | 0 / 336 | 0 / 336 | 0 / 336 | 0 / 336 | — | 1 | 1679 | 1680 |
| `chaos` | 0 / 336 | 0 / 336 | 0 / 336 | 0 / 336 | 0 / 336 | — | 0 | 1680 | 1680 |
| `custom` | 1 / 336 | 0 / 336 | 0 / 336 | 0 / 336 | 1 / 336 | — | 2 | 1678 | 1680 |
| `naked` | — | — | — | — | — | 0 / 336 | 0 | 336 | 336 |
| **Total** | | | | | | | **170** | **6886** | **7056** |

Ordinary full validation executes 172 selected regression cells: the 170 green compatibility cells above plus 2 chaos-mode race-exposure checks. A passing validate must produce a fresh result for all of them; a failing green cell is a regression, not permission to move it to red.
