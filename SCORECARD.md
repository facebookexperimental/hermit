# Compatibility scorecard

This table is derived from the manifest, not from a separately maintained parent-workspace CSV. `./ci/compat-envelope/scorecard.rs check` verifies it.

The count table includes all **5760** cells in the manifest; no row is omitted. A cell is **Selected by full** exactly when it appears in `ci/expected-e2e-plan.json`. A cell is **Not selected by full** when it is in the manifest but absent from that plan. Selection is not a test result: a cell not selected by full may have passed, failed, produced no verdict, or never run. Of these cells, **849** are selected by full, **154** are not selected by full, and **4757** are **Not applicable**.

Every selected `verify` cell, and every seed in a selected `chaos` cell, runs the same backend twice. The manifest runner adds `--verify-strict` when the selected Hermit binary supports it, and accepts a result only when the typed report says `verified=true`, `verdict=matched`, `bitwise_parity=true`, `strictness=canonical`, `compare_logs=true`, a named canonical `record_envelope`, and both INFO-message counts are nonzero. Bare `--verify` remains a Stripped comparison when invoked directly and does not satisfy this regression plan. These same-backend results do not establish cross-backend parity.

| Backend | Selected by full | Not selected by full | Not applicable | In the manifest |
| --- | ---: | ---: | ---: | ---: |
| `ptrace` | 348 | 17 | 715 | 1080 |
| `dbt` | 0 | 61 | 1019 | 1080 |
| `kvm` | 243 | 8 | 829 | 1080 |
| `sabre` | 112 | 32 | 936 | 1080 |
| `liteinst` | 146 | 3 | 931 | 1080 |
| `native` | 0 | 33 | 327 | 360 |
| **Total** | **849** | **154** | **4757** | **5760** |

## Denominator, and why the percentage is not comparable across changes to it

Selected by full is **849 of 5760**, which is **14.74%** — over THIS population and no other. The population is every combination the manifest declares, and it is composed of:

- backends: `ptrace`, `dbt`, `kvm`, `sabre`, `liteinst`, `native`
- modes: `chaos`, `naked`, `replay`, `verify`

⚠️ **4757 of those 5760 cells are NOT APPLICABLE** — their backend is not applicable for their mode, so they were never asked to run and cannot pass or fail. Over the 1003 cells that CAN run, selected by full is **84.65%**.

⚠️ **DO NOT QUOTE THAT SECOND FIGURE AS PROGRESS.** It is the same 849 cells selected by full measured against a smaller denominator. Nothing was fixed to produce it; it is what the first figure always meant once the cells that cannot run are excluded. Quote both or neither, and never compare one against the other as though something moved.

⚠️ **Adding or removing a backend or mode changes this denominator and therefore the percentage, without anything about the product changing.** Removing a backend whose cells are mostly not selected RAISES the reported figure; adding manifest cells that are not selected LOWERS it. Neither is progress. Before comparing this percentage against an earlier one, diff the two lists above: if they differ, the numbers are not comparable and the difference is not a result.

The mode view makes the current order of work explicit: expand `verify` first, then `replay`, then `chaos`. Each backend cell is `selected by full / in the manifest`; an em dash means that mode does not exist for that backend. The summary columns use the same selection and applicability facts as the table above.

| Mode | `ptrace` | `dbt` | `kvm` | `sabre` | `liteinst` | `native` | Selected by full | Not selected by full | Not applicable | In the manifest |
| --- | ---: | ---: | ---: | ---: | ---: | ---: | ---: | ---: | ---: | ---: |
| `verify` | 341 / 360 | 0 / 360 | 243 / 360 | 112 / 360 | 146 / 360 | — | 842 | 120 | 838 | 1800 |
| `replay` | 1 / 360 | 0 / 360 | 0 / 360 | 0 / 360 | 0 / 360 | — | 1 | 0 | 1799 | 1800 |
| `chaos` | 6 / 360 | 0 / 360 | 0 / 360 | 0 / 360 | 0 / 360 | — | 6 | 1 | 1793 | 1800 |
| `naked` | — | — | — | — | — | 0 / 360 | 0 | 33 | 327 | 360 |
| **Total** | | | | | | | **849** | **154** | **4757** | **5760** |

## Ptrace by manifest category

This view uses the same Basic Sanity Milestone 1 contracts as the tables above, but makes the ptrace workload mix visible. Each entry is `selected by full / in the manifest`; `custom` commands are not part of this denominator.

| Manifest category | Verify | Replay | Chaos | Selected by full | In the manifest |
| --- | ---: | ---: | ---: | ---: | ---: |
| `applications` | 3 / 6 | 0 / 6 | 0 / 6 | 3 | 18 |
| `backend-parity-c` | 103 / 104 | 0 / 104 | 0 / 104 | 103 | 312 |
| `bin-c` | 1 / 2 | 0 / 2 | 0 / 2 | 1 | 6 |
| `c-programs` | 159 / 165 | 0 / 165 | 3 / 165 | 162 | 495 |
| `chaos-c` | 1 / 1 | 0 / 1 | 1 / 1 | 2 | 3 |
| `data-handling` | 6 / 6 | 0 / 6 | 0 / 6 | 6 | 18 |
| `debugger-c` | 1 / 1 | 0 / 1 | 0 / 1 | 1 | 3 |
| `determinism-stress` | 5 / 6 | 0 / 6 | 1 / 6 | 6 | 18 |
| `determinism-stress-c` | 11 / 11 | 0 / 11 | 1 / 11 | 12 | 33 |
| `language-runtimes` | 18 / 19 | 0 / 19 | 0 / 19 | 18 | 57 |
| `shared-futex-c` | 0 / 4 | 0 / 4 | 0 / 4 | 0 | 12 |
| `system-utils` | 33 / 34 | 1 / 34 | 0 / 34 | 34 | 102 |
| `util-c` | 0 / 1 | 0 / 1 | 0 / 1 | 0 | 3 |

Ordinary full validation executes 852 cells: the 849 comparable compatibility cells selected by full above (including 6 chaos-mode race-exposure checks), and 3 explicit custom commands outside the comparable denominator. A passing validate must produce a fresh result for all of them; a failing selected cell is a regression, not permission to remove it from the plan.

### Selected custom commands outside the comparable denominator

These rows are part of the selected regression denominator even though they are not rows in `ci/compat-envelope/cells.json`. Their exact identities come from `ci/expected-e2e-plan.json`; `scorecard.rs check` refuses any selected row that is not accounted for by either this table or the comparable green cells above.

| Lane | Category | Test | Mode | Backend |
| --- | --- | --- | --- | --- |
| `portable` | `backend-parity-c` | `backend-parity-c/environment-and-workdir` | `custom` | `ptrace` |
| `portable` | `system-utils` | `system-utils/clock-determinism` | `custom` | `liteinst` |
| `portable` | `system-utils` | `system-utils/clock-determinism` | `custom` | `ptrace` |

## Cross-backend parity

This is measured ptrace-reference parity, not CI plan membership and not same-backend repeatability. A cell is eligible when the corresponding ptrace `verify` coordinate is selected by full. The CLI can explicitly select eligible not-applicable candidates with `--probe-disabled`; the committed selectors do not include that option. `Never measured` means no strict typed ptrace-vs-candidate report exists. At the latest recorded Hermit source depth, any divergence outranks a match. The portable and hosted-portable `backend-parity-c` nodes perform ptrace-reference parity comparisons for eligible selected verify cells. These selectors cover a subset of the eligible cells; eligibility does not mean every cell was selected or measured.

| Candidate backend | Ptrace cells selected by full | Not-applicable probe candidates | Measured match | Parity failure | Never measured |
| --- | ---: | ---: | ---: | ---: | ---: |
| `dbt` | 341 | 281 | 0 | 0 | 341 |
| `kvm` | 341 | 93 | 0 | 0 | 341 |
| `sabre` | 341 | 197 | 0 | 0 | 341 |
| `liteinst` | 341 | 194 | 0 | 0 | 341 |

Measured pairs are listed individually so a failing backend/test coordinate is visible without interpreting the plan-colour tables. The raw-record column is the smaller of the two complete input record counts, before target and INFO selection. The Ptrace INFO and Candidate INFO columns count the selected Detcore messages used for comparison.

| Test | Candidate backend | Result | Smaller raw record count | Ptrace INFO | Candidate INFO |
| --- | --- | --- | ---: | ---: | ---: |
| _none_ | — | — | — | — | — |

## Selection and measurement

Selection and observation answer different questions. The first column says whether full validation selects a cell. The per-cell `measurement` value says what retained evidence observed: `never-measured`, `measured-and-passed`, `measured-no-verdict`, `diverged-unlocated`, or `diverged`. Of the cells selected by full, **15** have `never-measured`; of the cells not selected by full, **70** have `measured-and-passed`.

Retained history that has not been imported is not counted here. A stored measurement does not establish that it describes current code; `show` reports whether the recorded last test still matches `HEAD:detcore`.

The count table includes all **5760** cells in the manifest; no row is omitted. These claims use the same counts printed in the table below.

| Selection by full | `never-measured` | `measured-and-passed` | `measured-no-verdict` | `diverged-unlocated` | `diverged` | In the manifest |
| --- | ---: | ---: | ---: | ---: | ---: | ---: |
| Selected by full | 15 | 814 | 0 | 6 | 14 | 849 |
| Not selected by full | 48 | 70 | 8 | 0 | 28 | 154 |
| Not applicable | 4756 | 0 | 0 | 0 | 1 | 4757 |
| **Total** | **4819** | **884** | **8** | **6** | **43** | **5760** |

Cells whose stored `measurement` is not `never-measured` are shown individually so selection and measurement remain visible together.

| Test | Mode | Backend | Selection by full | Measurement |
| --- | --- | --- | --- | --- |
| `applications/c-toolchain-workflow` | `verify` | `ptrace` | `Selected by full` | `measured-and-passed` |
| `applications/example-timed-progress-bar` | `verify` | `dbt` | `Not selected by full` | `measured-and-passed` |
| `applications/example-timed-progress-bar` | `verify` | `kvm` | `Not selected by full` | `measured-and-passed` |
| `applications/example-timed-progress-bar` | `verify` | `ptrace` | `Not selected by full` | `measured-and-passed` |
| `applications/git-repository-workflow` | `verify` | `ptrace` | `Selected by full` | `diverged-unlocated` |
| `applications/timed-progress-bar` | `verify` | `kvm` | `Selected by full` | `measured-and-passed` |
| `applications/timed-progress-bar` | `verify` | `ptrace` | `Selected by full` | `measured-and-passed` |
| `backend-parity-c/aio-refusal` | `verify` | `kvm` | `Selected by full` | `measured-and-passed` |
| `backend-parity-c/aio-refusal` | `verify` | `liteinst` | `Selected by full` | `measured-and-passed` |
| `backend-parity-c/aio-refusal` | `verify` | `ptrace` | `Selected by full` | `measured-and-passed` |
| `backend-parity-c/append-pwrite` | `verify` | `kvm` | `Selected by full` | `measured-and-passed` |
| `backend-parity-c/append-pwrite` | `verify` | `liteinst` | `Selected by full` | `measured-and-passed` |
| `backend-parity-c/append-pwrite` | `verify` | `ptrace` | `Selected by full` | `measured-and-passed` |
| `backend-parity-c/bind-getsockname` | `verify` | `kvm` | `Selected by full` | `measured-and-passed` |
| `backend-parity-c/bind-getsockname` | `verify` | `liteinst` | `Selected by full` | `measured-and-passed` |
| `backend-parity-c/bind-getsockname` | `verify` | `ptrace` | `Selected by full` | `measured-and-passed` |
| `backend-parity-c/cachestat-refusal` | `verify` | `kvm` | `Selected by full` | `measured-and-passed` |
| `backend-parity-c/cachestat-refusal` | `verify` | `liteinst` | `Selected by full` | `measured-and-passed` |
| `backend-parity-c/cachestat-refusal` | `verify` | `ptrace` | `Selected by full` | `measured-and-passed` |
| `backend-parity-c/child-subreaper-refusal` | `verify` | `kvm` | `Selected by full` | `measured-and-passed` |
| `backend-parity-c/child-subreaper-refusal` | `verify` | `liteinst` | `Selected by full` | `measured-and-passed` |
| `backend-parity-c/child-subreaper-refusal` | `verify` | `ptrace` | `Selected by full` | `measured-and-passed` |
| `backend-parity-c/close-range-fds` | `verify` | `kvm` | `Selected by full` | `measured-and-passed` |
| `backend-parity-c/close-range-fds` | `verify` | `liteinst` | `Selected by full` | `measured-and-passed` |
| `backend-parity-c/close-range-fds` | `verify` | `ptrace` | `Selected by full` | `measured-and-passed` |
| `backend-parity-c/copy-file-range-refusal` | `verify` | `kvm` | `Selected by full` | `measured-and-passed` |
| `backend-parity-c/copy-file-range-refusal` | `verify` | `liteinst` | `Selected by full` | `measured-and-passed` |
| `backend-parity-c/copy-file-range-refusal` | `verify` | `ptrace` | `Selected by full` | `measured-and-passed` |
| `backend-parity-c/cpu-virtualization` | `verify` | `dbt` | `Not selected by full` | `measured-and-passed` |
| `backend-parity-c/cpu-virtualization` | `verify` | `kvm` | `Selected by full` | `measured-and-passed` |
| `backend-parity-c/cpu-virtualization` | `verify` | `liteinst` | `Selected by full` | `measured-and-passed` |
| `backend-parity-c/cpu-virtualization` | `verify` | `ptrace` | `Selected by full` | `measured-and-passed` |
| `backend-parity-c/cwd-roundtrip` | `verify` | `kvm` | `Selected by full` | `measured-and-passed` |
| `backend-parity-c/cwd-roundtrip` | `verify` | `liteinst` | `Selected by full` | `measured-and-passed` |
| `backend-parity-c/cwd-roundtrip` | `verify` | `ptrace` | `Selected by full` | `measured-and-passed` |
| `backend-parity-c/dup-shared-offset` | `verify` | `dbt` | `Not selected by full` | `diverged` |
| `backend-parity-c/dup-shared-offset` | `verify` | `kvm` | `Selected by full` | `measured-and-passed` |
| `backend-parity-c/dup-shared-offset` | `verify` | `liteinst` | `Selected by full` | `measured-and-passed` |
| `backend-parity-c/dup-shared-offset` | `verify` | `ptrace` | `Selected by full` | `measured-and-passed` |
| `backend-parity-c/environment-and-workdir` | `verify` | `ptrace` | `Selected by full` | `measured-and-passed` |
| `backend-parity-c/epoll-pwait2` | `verify` | `liteinst` | `Selected by full` | `measured-and-passed` |
| `backend-parity-c/epoll-pwait2` | `verify` | `ptrace` | `Selected by full` | `measured-and-passed` |
| `backend-parity-c/epoll-readiness` | `verify` | `kvm` | `Selected by full` | `measured-and-passed` |
| `backend-parity-c/epoll-readiness` | `verify` | `liteinst` | `Selected by full` | `measured-and-passed` |
| `backend-parity-c/epoll-readiness` | `verify` | `ptrace` | `Selected by full` | `measured-and-passed` |
| `backend-parity-c/event-delivery-ordering` | `verify` | `liteinst` | `Selected by full` | `measured-and-passed` |
| `backend-parity-c/event-delivery-ordering` | `verify` | `ptrace` | `Selected by full` | `measured-and-passed` |
| `backend-parity-c/eventfd-semantics` | `verify` | `kvm` | `Selected by full` | `measured-and-passed` |
| `backend-parity-c/eventfd-semantics` | `verify` | `liteinst` | `Selected by full` | `measured-and-passed` |
| `backend-parity-c/eventfd-semantics` | `verify` | `ptrace` | `Selected by full` | `measured-and-passed` |
| `backend-parity-c/faccessat2-flags` | `verify` | `kvm` | `Selected by full` | `measured-and-passed` |
| `backend-parity-c/faccessat2-flags` | `verify` | `liteinst` | `Selected by full` | `measured-and-passed` |
| `backend-parity-c/faccessat2-flags` | `verify` | `ptrace` | `Selected by full` | `measured-and-passed` |
| `backend-parity-c/fadvise-hints` | `verify` | `kvm` | `Selected by full` | `measured-and-passed` |
| `backend-parity-c/fadvise-hints` | `verify` | `liteinst` | `Selected by full` | `measured-and-passed` |
| `backend-parity-c/fadvise-hints` | `verify` | `ptrace` | `Selected by full` | `measured-and-passed` |
| `backend-parity-c/fallocate-extents` | `verify` | `kvm` | `Selected by full` | `measured-and-passed` |
| `backend-parity-c/fallocate-extents` | `verify` | `liteinst` | `Selected by full` | `measured-and-passed` |
| `backend-parity-c/fallocate-extents` | `verify` | `ptrace` | `Selected by full` | `measured-and-passed` |
| `backend-parity-c/fchmod-bits` | `verify` | `kvm` | `Selected by full` | `measured-and-passed` |
| `backend-parity-c/fchmod-bits` | `verify` | `liteinst` | `Selected by full` | `measured-and-passed` |
| `backend-parity-c/fchmod-bits` | `verify` | `ptrace` | `Selected by full` | `measured-and-passed` |
| `backend-parity-c/fchmodat2-flags` | `verify` | `liteinst` | `Selected by full` | `measured-and-passed` |
| `backend-parity-c/fchmodat2-flags` | `verify` | `ptrace` | `Selected by full` | `measured-and-passed` |
| `backend-parity-c/fcntl-owner` | `verify` | `liteinst` | `Selected by full` | `measured-and-passed` |
| `backend-parity-c/fcntl-owner` | `verify` | `ptrace` | `Selected by full` | `measured-and-passed` |
| `backend-parity-c/fd-duplication` | `verify` | `dbt` | `Not selected by full` | `diverged` |
| `backend-parity-c/fd-duplication` | `verify` | `kvm` | `Selected by full` | `measured-and-passed` |
| `backend-parity-c/fd-duplication` | `verify` | `liteinst` | `Selected by full` | `measured-and-passed` |
| `backend-parity-c/fd-duplication` | `verify` | `ptrace` | `Selected by full` | `measured-and-passed` |
| `backend-parity-c/file-backed-mmap` | `verify` | `kvm` | `Selected by full` | `measured-and-passed` |
| `backend-parity-c/file-backed-mmap` | `verify` | `liteinst` | `Selected by full` | `measured-and-passed` |
| `backend-parity-c/file-backed-mmap` | `verify` | `ptrace` | `Selected by full` | `measured-and-passed` |
| `backend-parity-c/file-io-roundtrip` | `verify` | `kvm` | `Selected by full` | `measured-and-passed` |
| `backend-parity-c/file-io-roundtrip` | `verify` | `liteinst` | `Selected by full` | `measured-and-passed` |
| `backend-parity-c/file-io-roundtrip` | `verify` | `ptrace` | `Selected by full` | `measured-and-passed` |
| `backend-parity-c/flock-lifecycle` | `verify` | `kvm` | `Selected by full` | `measured-and-passed` |
| `backend-parity-c/flock-lifecycle` | `verify` | `liteinst` | `Selected by full` | `measured-and-passed` |
| `backend-parity-c/flock-lifecycle` | `verify` | `ptrace` | `Selected by full` | `measured-and-passed` |
| `backend-parity-c/fork-exec-pipeline` | `verify` | `kvm` | `Selected by full` | `diverged` |
| `backend-parity-c/fork-exec-pipeline` | `verify` | `ptrace` | `Selected by full` | `measured-and-passed` |
| `backend-parity-c/fsync-durability` | `verify` | `liteinst` | `Selected by full` | `measured-and-passed` |
| `backend-parity-c/fsync-durability` | `verify` | `ptrace` | `Selected by full` | `measured-and-passed` |
| `backend-parity-c/ftruncate-sparse` | `verify` | `kvm` | `Selected by full` | `measured-and-passed` |
| `backend-parity-c/ftruncate-sparse` | `verify` | `liteinst` | `Selected by full` | `measured-and-passed` |
| `backend-parity-c/ftruncate-sparse` | `verify` | `ptrace` | `Selected by full` | `measured-and-passed` |
| `backend-parity-c/getcpu-identity` | `verify` | `dbt` | `Not selected by full` | `measured-and-passed` |
| `backend-parity-c/getcpu-identity` | `verify` | `kvm` | `Selected by full` | `measured-and-passed` |
| `backend-parity-c/getcpu-identity` | `verify` | `liteinst` | `Selected by full` | `measured-and-passed` |
| `backend-parity-c/getcpu-identity` | `verify` | `ptrace` | `Selected by full` | `measured-and-passed` |
| `backend-parity-c/getpriority-identity` | `verify` | `dbt` | `Not selected by full` | `measured-and-passed` |
| `backend-parity-c/getpriority-identity` | `verify` | `kvm` | `Selected by full` | `measured-and-passed` |
| `backend-parity-c/getpriority-identity` | `verify` | `liteinst` | `Selected by full` | `measured-and-passed` |
| `backend-parity-c/getpriority-identity` | `verify` | `ptrace` | `Selected by full` | `measured-and-passed` |
| `backend-parity-c/getrusage-self-accounting` | `verify` | `liteinst` | `Not selected by full` | `measured-and-passed` |
| `backend-parity-c/getrusage-self-accounting` | `verify` | `ptrace` | `Selected by full` | `measured-and-passed` |
| `backend-parity-c/hardware-trap-identity` | `verify` | `dbt` | `Not selected by full` | `diverged` |
| `backend-parity-c/hardware-trap-identity` | `verify` | `liteinst` | `Selected by full` | `measured-and-passed` |
| `backend-parity-c/hardware-trap-identity` | `verify` | `ptrace` | `Selected by full` | `measured-and-passed` |
| `backend-parity-c/host-identity` | `verify` | `dbt` | `Not selected by full` | `measured-and-passed` |
| `backend-parity-c/host-identity` | `verify` | `liteinst` | `Selected by full` | `measured-and-passed` |
| `backend-parity-c/host-identity` | `verify` | `ptrace` | `Selected by full` | `measured-and-passed` |
| `backend-parity-c/inline-syscall-sites` | `verify` | `kvm` | `Selected by full` | `measured-and-passed` |
| `backend-parity-c/inline-syscall-sites` | `verify` | `liteinst` | `Selected by full` | `measured-and-passed` |
| `backend-parity-c/inline-syscall-sites` | `verify` | `ptrace` | `Selected by full` | `measured-and-passed` |
| `backend-parity-c/inotify-watch` | `verify` | `liteinst` | `Selected by full` | `measured-and-passed` |
| `backend-parity-c/inotify-watch` | `verify` | `ptrace` | `Selected by full` | `measured-and-passed` |
| `backend-parity-c/ioctl-fionread` | `verify` | `liteinst` | `Selected by full` | `measured-and-passed` |
| `backend-parity-c/ioctl-fionread` | `verify` | `ptrace` | `Selected by full` | `measured-and-passed` |
| `backend-parity-c/kcmp-refusal` | `verify` | `kvm` | `Selected by full` | `measured-and-passed` |
| `backend-parity-c/kcmp-refusal` | `verify` | `liteinst` | `Selected by full` | `measured-and-passed` |
| `backend-parity-c/kcmp-refusal` | `verify` | `ptrace` | `Selected by full` | `measured-and-passed` |
| `backend-parity-c/linkat-flags` | `verify` | `kvm` | `Selected by full` | `measured-and-passed` |
| `backend-parity-c/linkat-flags` | `verify` | `liteinst` | `Selected by full` | `measured-and-passed` |
| `backend-parity-c/linkat-flags` | `verify` | `ptrace` | `Selected by full` | `measured-and-passed` |
| `backend-parity-c/lseek-positioning` | `verify` | `dbt` | `Not selected by full` | `measured-and-passed` |
| `backend-parity-c/lseek-positioning` | `verify` | `kvm` | `Selected by full` | `measured-and-passed` |
| `backend-parity-c/lseek-positioning` | `verify` | `liteinst` | `Selected by full` | `measured-and-passed` |
| `backend-parity-c/lseek-positioning` | `verify` | `ptrace` | `Selected by full` | `measured-and-passed` |
| `backend-parity-c/mce-kill-refusal` | `verify` | `kvm` | `Selected by full` | `measured-and-passed` |
| `backend-parity-c/mce-kill-refusal` | `verify` | `liteinst` | `Selected by full` | `measured-and-passed` |
| `backend-parity-c/mce-kill-refusal` | `verify` | `ptrace` | `Selected by full` | `measured-and-passed` |
| `backend-parity-c/membarrier-query` | `verify` | `kvm` | `Selected by full` | `measured-and-passed` |
| `backend-parity-c/membarrier-query` | `verify` | `liteinst` | `Selected by full` | `measured-and-passed` |
| `backend-parity-c/membarrier-query` | `verify` | `ptrace` | `Selected by full` | `measured-and-passed` |
| `backend-parity-c/memfd-create` | `verify` | `kvm` | `Selected by full` | `measured-and-passed` |
| `backend-parity-c/memfd-create` | `verify` | `liteinst` | `Selected by full` | `measured-and-passed` |
| `backend-parity-c/memfd-create` | `verify` | `ptrace` | `Selected by full` | `measured-and-passed` |
| `backend-parity-c/mempolicy-default` | `verify` | `kvm` | `Selected by full` | `measured-and-passed` |
| `backend-parity-c/mempolicy-default` | `verify` | `liteinst` | `Selected by full` | `measured-and-passed` |
| `backend-parity-c/mempolicy-default` | `verify` | `ptrace` | `Selected by full` | `measured-and-passed` |
| `backend-parity-c/mincore-residency` | `verify` | `kvm` | `Selected by full` | `measured-and-passed` |
| `backend-parity-c/mincore-residency` | `verify` | `liteinst` | `Selected by full` | `measured-and-passed` |
| `backend-parity-c/mincore-residency` | `verify` | `ptrace` | `Selected by full` | `measured-and-passed` |
| `backend-parity-c/mixed-inline-and-libc-syscalls` | `verify` | `kvm` | `Selected by full` | `measured-and-passed` |
| `backend-parity-c/mixed-inline-and-libc-syscalls` | `verify` | `liteinst` | `Selected by full` | `measured-and-passed` |
| `backend-parity-c/mixed-inline-and-libc-syscalls` | `verify` | `ptrace` | `Selected by full` | `measured-and-passed` |
| `backend-parity-c/mkdir-rmdir` | `verify` | `kvm` | `Selected by full` | `measured-and-passed` |
| `backend-parity-c/mkdir-rmdir` | `verify` | `liteinst` | `Selected by full` | `measured-and-passed` |
| `backend-parity-c/mkdir-rmdir` | `verify` | `ptrace` | `Selected by full` | `measured-and-passed` |
| `backend-parity-c/mknod-special` | `verify` | `kvm` | `Selected by full` | `measured-and-passed` |
| `backend-parity-c/mknod-special` | `verify` | `liteinst` | `Selected by full` | `measured-and-passed` |
| `backend-parity-c/mknod-special` | `verify` | `ptrace` | `Selected by full` | `measured-and-passed` |
| `backend-parity-c/mmap-layout-pointer-order` | `verify` | `kvm` | `Selected by full` | `measured-and-passed` |
| `backend-parity-c/mmap-layout-pointer-order` | `verify` | `liteinst` | `Selected by full` | `measured-and-passed` |
| `backend-parity-c/mmap-layout-pointer-order` | `verify` | `ptrace` | `Selected by full` | `measured-and-passed` |
| `backend-parity-c/msync-writeback` | `verify` | `liteinst` | `Selected by full` | `measured-and-passed` |
| `backend-parity-c/msync-writeback` | `verify` | `ptrace` | `Selected by full` | `measured-and-passed` |
| `backend-parity-c/name-to-handle-refusal` | `verify` | `kvm` | `Selected by full` | `measured-and-passed` |
| `backend-parity-c/name-to-handle-refusal` | `verify` | `liteinst` | `Selected by full` | `measured-and-passed` |
| `backend-parity-c/name-to-handle-refusal` | `verify` | `ptrace` | `Selected by full` | `measured-and-passed` |
| `backend-parity-c/no-new-privs-refusal` | `verify` | `kvm` | `Selected by full` | `measured-and-passed` |
| `backend-parity-c/no-new-privs-refusal` | `verify` | `liteinst` | `Selected by full` | `measured-and-passed` |
| `backend-parity-c/no-new-privs-refusal` | `verify` | `ptrace` | `Selected by full` | `measured-and-passed` |
| `backend-parity-c/numa-node-identity` | `verify` | `dbt` | `Not selected by full` | `measured-and-passed` |
| `backend-parity-c/numa-node-identity` | `verify` | `kvm` | `Selected by full` | `measured-and-passed` |
| `backend-parity-c/numa-node-identity` | `verify` | `liteinst` | `Selected by full` | `measured-and-passed` |
| `backend-parity-c/numa-node-identity` | `verify` | `ptrace` | `Selected by full` | `measured-and-passed` |
| `backend-parity-c/o-tmpfile-anon` | `verify` | `kvm` | `Selected by full` | `measured-and-passed` |
| `backend-parity-c/o-tmpfile-anon` | `verify` | `liteinst` | `Selected by full` | `measured-and-passed` |
| `backend-parity-c/o-tmpfile-anon` | `verify` | `ptrace` | `Selected by full` | `measured-and-passed` |
| `backend-parity-c/openat-flags` | `verify` | `kvm` | `Selected by full` | `measured-and-passed` |
| `backend-parity-c/openat-flags` | `verify` | `liteinst` | `Selected by full` | `measured-and-passed` |
| `backend-parity-c/openat-flags` | `verify` | `ptrace` | `Selected by full` | `measured-and-passed` |
| `backend-parity-c/openat2-refusal` | `verify` | `kvm` | `Selected by full` | `measured-and-passed` |
| `backend-parity-c/openat2-refusal` | `verify` | `liteinst` | `Selected by full` | `measured-and-passed` |
| `backend-parity-c/openat2-refusal` | `verify` | `ptrace` | `Selected by full` | `measured-and-passed` |
| `backend-parity-c/path-file-ops` | `verify` | `kvm` | `Selected by full` | `measured-and-passed` |
| `backend-parity-c/path-file-ops` | `verify` | `liteinst` | `Selected by full` | `measured-and-passed` |
| `backend-parity-c/path-file-ops` | `verify` | `ptrace` | `Selected by full` | `measured-and-passed` |
| `backend-parity-c/personality-domain` | `verify` | `liteinst` | `Selected by full` | `measured-and-passed` |
| `backend-parity-c/personality-domain` | `verify` | `ptrace` | `Selected by full` | `measured-and-passed` |
| `backend-parity-c/pid-probe` | `verify` | `kvm` | `Selected by full` | `measured-and-passed` |
| `backend-parity-c/pid-probe` | `verify` | `liteinst` | `Selected by full` | `measured-and-passed` |
| `backend-parity-c/pid-probe` | `verify` | `ptrace` | `Selected by full` | `measured-and-passed` |
| `backend-parity-c/pid-probe` | `verify` | `sabre` | `Selected by full` | `measured-and-passed` |
| `backend-parity-c/pidfd-open-self` | `verify` | `dbt` | `Not selected by full` | `measured-and-passed` |
| `backend-parity-c/pidfd-open-self` | `verify` | `kvm` | `Selected by full` | `measured-and-passed` |
| `backend-parity-c/pidfd-open-self` | `verify` | `liteinst` | `Selected by full` | `measured-and-passed` |
| `backend-parity-c/pidfd-open-self` | `verify` | `ptrace` | `Selected by full` | `measured-and-passed` |
| `backend-parity-c/pipe-capacity` | `verify` | `kvm` | `Selected by full` | `measured-and-passed` |
| `backend-parity-c/pipe-capacity` | `verify` | `liteinst` | `Selected by full` | `measured-and-passed` |
| `backend-parity-c/pipe-capacity` | `verify` | `ptrace` | `Selected by full` | `measured-and-passed` |
| `backend-parity-c/pipe-capacity-pin` | `verify` | `kvm` | `Selected by full` | `measured-and-passed` |
| `backend-parity-c/pipe-capacity-pin` | `verify` | `liteinst` | `Selected by full` | `measured-and-passed` |
| `backend-parity-c/pipe-capacity-pin` | `verify` | `ptrace` | `Selected by full` | `measured-and-passed` |
| `backend-parity-c/pipe-ipc` | `verify` | `kvm` | `Selected by full` | `measured-and-passed` |
| `backend-parity-c/pipe-ipc` | `verify` | `liteinst` | `Selected by full` | `measured-and-passed` |
| `backend-parity-c/pipe-ipc` | `verify` | `ptrace` | `Selected by full` | `measured-and-passed` |
| `backend-parity-c/pipe-multiwriter-ordering` | `verify` | `ptrace` | `Selected by full` | `measured-and-passed` |
| `backend-parity-c/pipe2-flags` | `verify` | `kvm` | `Selected by full` | `measured-and-passed` |
| `backend-parity-c/pipe2-flags` | `verify` | `liteinst` | `Selected by full` | `measured-and-passed` |
| `backend-parity-c/pipe2-flags` | `verify` | `ptrace` | `Selected by full` | `measured-and-passed` |
| `backend-parity-c/poll-readiness` | `verify` | `liteinst` | `Selected by full` | `measured-and-passed` |
| `backend-parity-c/poll-readiness` | `verify` | `ptrace` | `Selected by full` | `measured-and-passed` |
| `backend-parity-c/prctl-identity` | `verify` | `dbt` | `Not selected by full` | `measured-and-passed` |
| `backend-parity-c/prctl-identity` | `verify` | `liteinst` | `Selected by full` | `measured-and-passed` |
| `backend-parity-c/prctl-identity` | `verify` | `ptrace` | `Selected by full` | `measured-and-passed` |
| `backend-parity-c/prctl-pdeathsig` | `verify` | `liteinst` | `Selected by full` | `measured-and-passed` |
| `backend-parity-c/prctl-pdeathsig` | `verify` | `ptrace` | `Selected by full` | `measured-and-passed` |
| `backend-parity-c/preadv2-flags` | `verify` | `liteinst` | `Selected by full` | `measured-and-passed` |
| `backend-parity-c/preadv2-flags` | `verify` | `ptrace` | `Selected by full` | `measured-and-passed` |
| `backend-parity-c/pthread-lifecycle` | `verify` | `kvm` | `Not selected by full` | `diverged` |
| `backend-parity-c/pthread-lifecycle` | `verify` | `liteinst` | `Selected by full` | `measured-and-passed` |
| `backend-parity-c/pthread-lifecycle` | `verify` | `ptrace` | `Selected by full` | `measured-and-passed` |
| `backend-parity-c/pthread-lifecycle` | `verify` | `sabre` | `Not selected by full` | `diverged` |
| `backend-parity-c/readdir-entries` | `verify` | `kvm` | `Selected by full` | `measured-and-passed` |
| `backend-parity-c/readdir-entries` | `verify` | `liteinst` | `Selected by full` | `measured-and-passed` |
| `backend-parity-c/readdir-entries` | `verify` | `ptrace` | `Selected by full` | `measured-and-passed` |
| `backend-parity-c/readdir-order-identity` | `verify` | `kvm` | `Selected by full` | `measured-and-passed` |
| `backend-parity-c/readdir-order-identity` | `verify` | `liteinst` | `Selected by full` | `measured-and-passed` |
| `backend-parity-c/readdir-order-identity` | `verify` | `ptrace` | `Selected by full` | `measured-and-passed` |
| `backend-parity-c/record-lock` | `verify` | `liteinst` | `Selected by full` | `measured-and-passed` |
| `backend-parity-c/record-lock` | `verify` | `ptrace` | `Selected by full` | `measured-and-passed` |
| `backend-parity-c/rename-ops` | `verify` | `kvm` | `Selected by full` | `measured-and-passed` |
| `backend-parity-c/rename-ops` | `verify` | `liteinst` | `Selected by full` | `measured-and-passed` |
| `backend-parity-c/rename-ops` | `verify` | `ptrace` | `Selected by full` | `measured-and-passed` |
| `backend-parity-c/renameat2-flags` | `verify` | `kvm` | `Selected by full` | `measured-and-passed` |
| `backend-parity-c/renameat2-flags` | `verify` | `liteinst` | `Selected by full` | `measured-and-passed` |
| `backend-parity-c/renameat2-flags` | `verify` | `ptrace` | `Selected by full` | `measured-and-passed` |
| `backend-parity-c/rlimit-identity` | `verify` | `dbt` | `Not selected by full` | `measured-and-passed` |
| `backend-parity-c/rlimit-identity` | `verify` | `kvm` | `Selected by full` | `measured-and-passed` |
| `backend-parity-c/rlimit-identity` | `verify` | `liteinst` | `Selected by full` | `measured-and-passed` |
| `backend-parity-c/rlimit-identity` | `verify` | `ptrace` | `Selected by full` | `measured-and-passed` |
| `backend-parity-c/robust-list` | `verify` | `kvm` | `Selected by full` | `measured-and-passed` |
| `backend-parity-c/robust-list` | `verify` | `liteinst` | `Selected by full` | `measured-and-passed` |
| `backend-parity-c/robust-list` | `verify` | `ptrace` | `Selected by full` | `measured-and-passed` |
| `backend-parity-c/sched-getaffinity-identity` | `verify` | `dbt` | `Not selected by full` | `measured-and-passed` |
| `backend-parity-c/sched-getaffinity-identity` | `verify` | `kvm` | `Selected by full` | `measured-and-passed` |
| `backend-parity-c/sched-getaffinity-identity` | `verify` | `liteinst` | `Selected by full` | `measured-and-passed` |
| `backend-parity-c/sched-getaffinity-identity` | `verify` | `ptrace` | `Selected by full` | `measured-and-passed` |
| `backend-parity-c/seccomp-refusal` | `verify` | `kvm` | `Selected by full` | `measured-and-passed` |
| `backend-parity-c/seccomp-refusal` | `verify` | `liteinst` | `Selected by full` | `measured-and-passed` |
| `backend-parity-c/seccomp-refusal` | `verify` | `ptrace` | `Selected by full` | `measured-and-passed` |
| `backend-parity-c/sendfile-copy` | `verify` | `kvm` | `Selected by full` | `measured-and-passed` |
| `backend-parity-c/sendfile-copy` | `verify` | `liteinst` | `Selected by full` | `measured-and-passed` |
| `backend-parity-c/sendfile-copy` | `verify` | `ptrace` | `Selected by full` | `measured-and-passed` |
| `backend-parity-c/set-tid-address` | `verify` | `kvm` | `Selected by full` | `measured-and-passed` |
| `backend-parity-c/set-tid-address` | `verify` | `liteinst` | `Selected by full` | `measured-and-passed` |
| `backend-parity-c/set-tid-address` | `verify` | `ptrace` | `Selected by full` | `measured-and-passed` |
| `backend-parity-c/short-io-split-identity` | `verify` | `kvm` | `Selected by full` | `measured-and-passed` |
| `backend-parity-c/short-io-split-identity` | `verify` | `liteinst` | `Selected by full` | `measured-and-passed` |
| `backend-parity-c/short-io-split-identity` | `verify` | `ptrace` | `Selected by full` | `measured-and-passed` |
| `backend-parity-c/shutdown-socketpair` | `verify` | `kvm` | `Selected by full` | `measured-and-passed` |
| `backend-parity-c/shutdown-socketpair` | `verify` | `liteinst` | `Selected by full` | `measured-and-passed` |
| `backend-parity-c/shutdown-socketpair` | `verify` | `ptrace` | `Selected by full` | `measured-and-passed` |
| `backend-parity-c/signal-delivery-sequence` | `verify` | `liteinst` | `Selected by full` | `measured-and-passed` |
| `backend-parity-c/signal-delivery-sequence` | `verify` | `ptrace` | `Selected by full` | `measured-and-passed` |
| `backend-parity-c/signal-waitstatus-identity` | `verify` | `liteinst` | `Selected by full` | `measured-and-passed` |
| `backend-parity-c/signal-waitstatus-identity` | `verify` | `ptrace` | `Selected by full` | `measured-and-passed` |
| `backend-parity-c/signalfd-create` | `verify` | `kvm` | `Selected by full` | `measured-and-passed` |
| `backend-parity-c/signalfd-create` | `verify` | `liteinst` | `Selected by full` | `measured-and-passed` |
| `backend-parity-c/signalfd-create` | `verify` | `ptrace` | `Selected by full` | `measured-and-passed` |
| `backend-parity-c/socket-epoll-ordering` | `verify` | `kvm` | `Selected by full` | `measured-and-passed` |
| `backend-parity-c/socket-epoll-ordering` | `verify` | `liteinst` | `Selected by full` | `measured-and-passed` |
| `backend-parity-c/socket-epoll-ordering` | `verify` | `ptrace` | `Selected by full` | `measured-and-passed` |
| `backend-parity-c/socket-options` | `verify` | `liteinst` | `Selected by full` | `measured-and-passed` |
| `backend-parity-c/socket-options` | `verify` | `ptrace` | `Selected by full` | `measured-and-passed` |
| `backend-parity-c/socketpair-flags` | `verify` | `liteinst` | `Selected by full` | `measured-and-passed` |
| `backend-parity-c/socketpair-flags` | `verify` | `ptrace` | `Selected by full` | `measured-and-passed` |
| `backend-parity-c/sockname-unnamed` | `verify` | `liteinst` | `Selected by full` | `measured-and-passed` |
| `backend-parity-c/sockname-unnamed` | `verify` | `ptrace` | `Selected by full` | `measured-and-passed` |
| `backend-parity-c/stat-metadata-identity` | `verify` | `kvm` | `Selected by full` | `measured-and-passed` |
| `backend-parity-c/stat-metadata-identity` | `verify` | `liteinst` | `Selected by full` | `measured-and-passed` |
| `backend-parity-c/stat-metadata-identity` | `verify` | `ptrace` | `Selected by full` | `measured-and-passed` |
| `backend-parity-c/statfs-free-determinism` | `verify` | `kvm` | `Selected by full` | `measured-and-passed` |
| `backend-parity-c/statfs-free-determinism` | `verify` | `liteinst` | `Selected by full` | `measured-and-passed` |
| `backend-parity-c/statfs-free-determinism` | `verify` | `ptrace` | `Selected by full` | `measured-and-passed` |
| `backend-parity-c/static-nolibc-syscall-sites` | `verify` | `kvm` | `Selected by full` | `measured-and-passed` |
| `backend-parity-c/static-nolibc-syscall-sites` | `verify` | `ptrace` | `Selected by full` | `measured-and-passed` |
| `backend-parity-c/statx-metadata` | `verify` | `kvm` | `Selected by full` | `measured-and-passed` |
| `backend-parity-c/statx-metadata` | `verify` | `liteinst` | `Selected by full` | `measured-and-passed` |
| `backend-parity-c/statx-metadata` | `verify` | `ptrace` | `Selected by full` | `measured-and-passed` |
| `backend-parity-c/symlink-ops` | `verify` | `kvm` | `Selected by full` | `measured-and-passed` |
| `backend-parity-c/symlink-ops` | `verify` | `liteinst` | `Selected by full` | `measured-and-passed` |
| `backend-parity-c/symlink-ops` | `verify` | `ptrace` | `Selected by full` | `measured-and-passed` |
| `backend-parity-c/sync-file-range` | `verify` | `kvm` | `Selected by full` | `measured-and-passed` |
| `backend-parity-c/sync-file-range` | `verify` | `liteinst` | `Selected by full` | `measured-and-passed` |
| `backend-parity-c/sync-file-range` | `verify` | `ptrace` | `Selected by full` | `measured-and-passed` |
| `backend-parity-c/sysv-ipc-refusal` | `verify` | `kvm` | `Selected by full` | `measured-and-passed` |
| `backend-parity-c/sysv-ipc-refusal` | `verify` | `liteinst` | `Selected by full` | `measured-and-passed` |
| `backend-parity-c/sysv-ipc-refusal` | `verify` | `ptrace` | `Selected by full` | `measured-and-passed` |
| `backend-parity-c/thp-disable` | `verify` | `liteinst` | `Selected by full` | `measured-and-passed` |
| `backend-parity-c/thp-disable` | `verify` | `ptrace` | `Selected by full` | `measured-and-passed` |
| `backend-parity-c/timer-family-identity` | `verify` | `ptrace` | `Not selected by full` | `diverged` |
| `backend-parity-c/umask-mode` | `verify` | `kvm` | `Selected by full` | `measured-and-passed` |
| `backend-parity-c/umask-mode` | `verify` | `liteinst` | `Selected by full` | `measured-and-passed` |
| `backend-parity-c/umask-mode` | `verify` | `ptrace` | `Selected by full` | `measured-and-passed` |
| `backend-parity-c/uname-identity` | `verify` | `kvm` | `Selected by full` | `measured-and-passed` |
| `backend-parity-c/uname-identity` | `verify` | `liteinst` | `Selected by full` | `measured-and-passed` |
| `backend-parity-c/uname-identity` | `verify` | `ptrace` | `Selected by full` | `measured-and-passed` |
| `backend-parity-c/utimensat-determinism` | `verify` | `kvm` | `Selected by full` | `measured-and-passed` |
| `backend-parity-c/utimensat-determinism` | `verify` | `liteinst` | `Selected by full` | `measured-and-passed` |
| `backend-parity-c/utimensat-determinism` | `verify` | `ptrace` | `Selected by full` | `measured-and-passed` |
| `backend-parity-c/vectored-file-io` | `verify` | `liteinst` | `Selected by full` | `measured-and-passed` |
| `backend-parity-c/vectored-file-io` | `verify` | `ptrace` | `Selected by full` | `measured-and-passed` |
| `backend-parity-c/vectored-io` | `verify` | `kvm` | `Selected by full` | `measured-and-passed` |
| `backend-parity-c/vectored-io` | `verify` | `liteinst` | `Selected by full` | `measured-and-passed` |
| `backend-parity-c/vectored-io` | `verify` | `ptrace` | `Selected by full` | `measured-and-passed` |
| `bin-c/posix-timer-test` | `verify` | `liteinst` | `Selected by full` | `measured-and-passed` |
| `bin-c/posix-timer-test` | `verify` | `ptrace` | `Selected by full` | `measured-and-passed` |
| `bin-c/posix-timer-test` | `verify` | `sabre` | `Selected by full` | `measured-and-passed` |
| `c-programs/acct-refusal-probe` | `verify` | `dbt` | `Not selected by full` | `measured-and-passed` |
| `c-programs/acct-refusal-probe` | `verify` | `kvm` | `Selected by full` | `measured-and-passed` |
| `c-programs/acct-refusal-probe` | `verify` | `liteinst` | `Selected by full` | `measured-and-passed` |
| `c-programs/acct-refusal-probe` | `verify` | `ptrace` | `Selected by full` | `measured-and-passed` |
| `c-programs/acct-refusal-probe` | `verify` | `sabre` | `Selected by full` | `measured-and-passed` |
| `c-programs/add-key-enosys` | `verify` | `dbt` | `Not selected by full` | `measured-and-passed` |
| `c-programs/add-key-enosys` | `verify` | `kvm` | `Selected by full` | `measured-and-passed` |
| `c-programs/add-key-enosys` | `verify` | `liteinst` | `Selected by full` | `measured-and-passed` |
| `c-programs/add-key-enosys` | `verify` | `ptrace` | `Selected by full` | `measured-and-passed` |
| `c-programs/add-key-enosys` | `verify` | `sabre` | `Selected by full` | `measured-and-passed` |
| `c-programs/adjtimex-deterministic` | `verify` | `dbt` | `Not selected by full` | `measured-and-passed` |
| `c-programs/adjtimex-deterministic` | `verify` | `kvm` | `Selected by full` | `measured-and-passed` |
| `c-programs/adjtimex-deterministic` | `verify` | `liteinst` | `Selected by full` | `measured-and-passed` |
| `c-programs/adjtimex-deterministic` | `verify` | `ptrace` | `Selected by full` | `measured-and-passed` |
| `c-programs/adjtimex-deterministic` | `verify` | `sabre` | `Selected by full` | `measured-and-passed` |
| `c-programs/arch-prctl-determinism` | `verify` | `kvm` | `Selected by full` | `measured-and-passed` |
| `c-programs/arch-prctl-determinism` | `verify` | `ptrace` | `Selected by full` | `measured-and-passed` |
| `c-programs/bpf-enosys` | `verify` | `dbt` | `Not selected by full` | `measured-and-passed` |
| `c-programs/bpf-enosys` | `verify` | `kvm` | `Selected by full` | `measured-and-passed` |
| `c-programs/bpf-enosys` | `verify` | `liteinst` | `Selected by full` | `measured-and-passed` |
| `c-programs/bpf-enosys` | `verify` | `ptrace` | `Selected by full` | `measured-and-passed` |
| `c-programs/bpf-enosys` | `verify` | `sabre` | `Selected by full` | `measured-and-passed` |
| `c-programs/cachestat-enosys` | `verify` | `dbt` | `Not selected by full` | `measured-and-passed` |
| `c-programs/cachestat-enosys` | `verify` | `kvm` | `Selected by full` | `measured-and-passed` |
| `c-programs/cachestat-enosys` | `verify` | `liteinst` | `Selected by full` | `measured-and-passed` |
| `c-programs/cachestat-enosys` | `verify` | `ptrace` | `Selected by full` | `measured-and-passed` |
| `c-programs/cachestat-enosys` | `verify` | `sabre` | `Selected by full` | `measured-and-passed` |
| `c-programs/clock-adjtime-deterministic` | `verify` | `dbt` | `Not selected by full` | `measured-and-passed` |
| `c-programs/clock-adjtime-deterministic` | `verify` | `kvm` | `Selected by full` | `measured-and-passed` |
| `c-programs/clock-adjtime-deterministic` | `verify` | `liteinst` | `Selected by full` | `measured-and-passed` |
| `c-programs/clock-adjtime-deterministic` | `verify` | `ptrace` | `Selected by full` | `measured-and-passed` |
| `c-programs/clock-adjtime-deterministic` | `verify` | `sabre` | `Selected by full` | `measured-and-passed` |
| `c-programs/clone` | `verify` | `liteinst` | `Selected by full` | `measured-and-passed` |
| `c-programs/clone` | `verify` | `ptrace` | `Selected by full` | `measured-and-passed` |
| `c-programs/copy-file-range-refusal-probe` | `verify` | `dbt` | `Not selected by full` | `measured-and-passed` |
| `c-programs/copy-file-range-refusal-probe` | `verify` | `kvm` | `Selected by full` | `measured-and-passed` |
| `c-programs/copy-file-range-refusal-probe` | `verify` | `liteinst` | `Selected by full` | `measured-and-passed` |
| `c-programs/copy-file-range-refusal-probe` | `verify` | `ptrace` | `Selected by full` | `measured-and-passed` |
| `c-programs/copy-file-range-refusal-probe` | `verify` | `sabre` | `Selected by full` | `measured-and-passed` |
| `c-programs/dbt-copied-tiocgpgrp` | `verify` | `kvm` | `Selected by full` | `diverged` |
| `c-programs/dbt-copied-tiocgpgrp` | `verify` | `liteinst` | `Selected by full` | `measured-and-passed` |
| `c-programs/dbt-copied-tiocgpgrp` | `verify` | `ptrace` | `Selected by full` | `measured-and-passed` |
| `c-programs/dbt-copied-tiocgpgrp` | `verify` | `sabre` | `Not selected by full` | `measured-and-passed` |
| `c-programs/dbt-exec-failure` | `verify` | `kvm` | `Selected by full` | `measured-and-passed` |
| `c-programs/dbt-exec-failure` | `verify` | `liteinst` | `Selected by full` | `measured-and-passed` |
| `c-programs/dbt-exec-failure` | `verify` | `ptrace` | `Selected by full` | `measured-and-passed` |
| `c-programs/dbt-exec-failure` | `verify` | `sabre` | `Selected by full` | `measured-and-passed` |
| `c-programs/dbt-execveat-unsupported` | `verify` | `dbt` | `Not selected by full` | `measured-and-passed` |
| `c-programs/dbt-execveat-unsupported` | `verify` | `kvm` | `Selected by full` | `measured-and-passed` |
| `c-programs/dbt-execveat-unsupported` | `verify` | `ptrace` | `Selected by full` | `measured-and-passed` |
| `c-programs/dbt-execveat-unsupported` | `verify` | `sabre` | `Not selected by full` | `measured-no-verdict` |
| `c-programs/dbt-mmap-exec` | `verify` | `kvm` | `Selected by full` | `measured-and-passed` |
| `c-programs/dbt-mmap-exec` | `verify` | `liteinst` | `Selected by full` | `measured-and-passed` |
| `c-programs/dbt-mmap-exec` | `verify` | `ptrace` | `Selected by full` | `measured-and-passed` |
| `c-programs/dbt-mmap-exec` | `verify` | `sabre` | `Selected by full` | `measured-and-passed` |
| `c-programs/dbt-pid-virtualization` | `verify` | `ptrace` | `Not selected by full` | `measured-and-passed` |
| `c-programs/dbt-prlimit-self` | `verify` | `kvm` | `Selected by full` | `measured-and-passed` |
| `c-programs/dbt-prlimit-self` | `verify` | `liteinst` | `Selected by full` | `measured-and-passed` |
| `c-programs/dbt-prlimit-self` | `verify` | `ptrace` | `Selected by full` | `measured-and-passed` |
| `c-programs/dbt-prlimit-self` | `verify` | `sabre` | `Selected by full` | `measured-and-passed` |
| `c-programs/dbt-self-sigqueue` | `verify` | `liteinst` | `Selected by full` | `measured-and-passed` |
| `c-programs/dbt-self-sigqueue` | `verify` | `ptrace` | `Selected by full` | `measured-and-passed` |
| `c-programs/dbt-self-sigqueue` | `verify` | `sabre` | `Selected by full` | `measured-and-passed` |
| `c-programs/dbt-wait-lifecycle` | `verify` | `liteinst` | `Selected by full` | `measured-and-passed` |
| `c-programs/dbt-wait-lifecycle` | `verify` | `ptrace` | `Selected by full` | `measured-and-passed` |
| `c-programs/dbt-wait-lifecycle` | `verify` | `sabre` | `Not selected by full` | `measured-and-passed` |
| `c-programs/epoll-determinism` | `verify` | `kvm` | `Selected by full` | `measured-and-passed` |
| `c-programs/epoll-determinism` | `verify` | `liteinst` | `Selected by full` | `measured-and-passed` |
| `c-programs/epoll-determinism` | `verify` | `ptrace` | `Selected by full` | `measured-and-passed` |
| `c-programs/epoll-determinism` | `verify` | `sabre` | `Selected by full` | `measured-and-passed` |
| `c-programs/fp-reduction-nondeterminism` | `chaos` | `ptrace` | `Selected by full` | `diverged-unlocated` |
| `c-programs/fp-reduction-nondeterminism` | `verify` | `liteinst` | `Selected by full` | `measured-and-passed` |
| `c-programs/fp-reduction-nondeterminism` | `verify` | `ptrace` | `Selected by full` | `measured-and-passed` |
| `c-programs/futex-requeue-enosys` | `verify` | `dbt` | `Not selected by full` | `measured-and-passed` |
| `c-programs/futex-requeue-enosys` | `verify` | `kvm` | `Selected by full` | `measured-and-passed` |
| `c-programs/futex-requeue-enosys` | `verify` | `liteinst` | `Selected by full` | `measured-and-passed` |
| `c-programs/futex-requeue-enosys` | `verify` | `ptrace` | `Selected by full` | `measured-and-passed` |
| `c-programs/futex-requeue-enosys` | `verify` | `sabre` | `Selected by full` | `measured-and-passed` |
| `c-programs/futex-waitv-enosys` | `verify` | `dbt` | `Not selected by full` | `measured-and-passed` |
| `c-programs/futex-waitv-enosys` | `verify` | `kvm` | `Selected by full` | `measured-and-passed` |
| `c-programs/futex-waitv-enosys` | `verify` | `liteinst` | `Selected by full` | `measured-and-passed` |
| `c-programs/futex-waitv-enosys` | `verify` | `ptrace` | `Selected by full` | `measured-and-passed` |
| `c-programs/futex-waitv-enosys` | `verify` | `sabre` | `Selected by full` | `measured-and-passed` |
| `c-programs/futex-wake-enosys` | `verify` | `dbt` | `Not selected by full` | `measured-and-passed` |
| `c-programs/futex-wake-enosys` | `verify` | `kvm` | `Selected by full` | `measured-and-passed` |
| `c-programs/futex-wake-enosys` | `verify` | `liteinst` | `Selected by full` | `measured-and-passed` |
| `c-programs/futex-wake-enosys` | `verify` | `ptrace` | `Selected by full` | `measured-and-passed` |
| `c-programs/futex-wake-enosys` | `verify` | `sabre` | `Selected by full` | `measured-and-passed` |
| `c-programs/get-robust-list-child` | `verify` | `kvm` | `Selected by full` | `diverged` |
| `c-programs/get-robust-list-child` | `verify` | `liteinst` | `Selected by full` | `measured-and-passed` |
| `c-programs/get-robust-list-child` | `verify` | `ptrace` | `Selected by full` | `measured-and-passed` |
| `c-programs/get-robust-list-child` | `verify` | `sabre` | `Not selected by full` | `measured-and-passed` |
| `c-programs/get-robust-list-self` | `verify` | `dbt` | `Not selected by full` | `measured-and-passed` |
| `c-programs/get-robust-list-self` | `verify` | `kvm` | `Selected by full` | `measured-and-passed` |
| `c-programs/get-robust-list-self` | `verify` | `ptrace` | `Selected by full` | `measured-and-passed` |
| `c-programs/get-robust-list-self` | `verify` | `sabre` | `Selected by full` | `measured-and-passed` |
| `c-programs/get-robust-list-thread` | `verify` | `ptrace` | `Selected by full` | `measured-and-passed` |
| `c-programs/get-robust-list-thread` | `verify` | `sabre` | `Not selected by full` | `measured-no-verdict` |
| `c-programs/getcpu` | `verify` | `kvm` | `Selected by full` | `measured-and-passed` |
| `c-programs/getcpu` | `verify` | `ptrace` | `Selected by full` | `measured-and-passed` |
| `c-programs/getcpu` | `verify` | `sabre` | `Selected by full` | `measured-and-passed` |
| `c-programs/getitimer-determinism-probe` | `verify` | `kvm` | `Selected by full` | `measured-and-passed` |
| `c-programs/getitimer-determinism-probe` | `verify` | `ptrace` | `Selected by full` | `measured-and-passed` |
| `c-programs/getitimer-determinism-probe` | `verify` | `sabre` | `Selected by full` | `measured-and-passed` |
| `c-programs/getsockopt-null` | `verify` | `kvm` | `Selected by full` | `measured-and-passed` |
| `c-programs/getsockopt-null` | `verify` | `ptrace` | `Selected by full` | `measured-and-passed` |
| `c-programs/getsockopt-null` | `verify` | `sabre` | `Selected by full` | `measured-and-passed` |
| `c-programs/hello-alarm` | `verify` | `ptrace` | `Selected by full` | `measured-and-passed` |
| `c-programs/hello-alarm` | `verify` | `sabre` | `Selected by full` | `measured-and-passed` |
| `c-programs/hello-nostdlib` | `verify` | `kvm` | `Selected by full` | `measured-and-passed` |
| `c-programs/hello-nostdlib` | `verify` | `ptrace` | `Selected by full` | `measured-and-passed` |
| `c-programs/hello-signals` | `verify` | `ptrace` | `Selected by full` | `measured-and-passed` |
| `c-programs/hello-signals` | `verify` | `sabre` | `Selected by full` | `measured-and-passed` |
| `c-programs/io-uring-fallback` | `verify` | `kvm` | `Selected by full` | `measured-and-passed` |
| `c-programs/io-uring-fallback` | `verify` | `ptrace` | `Selected by full` | `measured-and-passed` |
| `c-programs/io-uring-fallback` | `verify` | `sabre` | `Selected by full` | `measured-and-passed` |
| `c-programs/io-uring-ring-determinism` | `verify` | `kvm` | `Selected by full` | `measured-and-passed` |
| `c-programs/io-uring-ring-determinism` | `verify` | `ptrace` | `Selected by full` | `measured-and-passed` |
| `c-programs/io-uring-ring-determinism` | `verify` | `sabre` | `Selected by full` | `measured-and-passed` |
| `c-programs/ioctl-fioclex` | `verify` | `dbt` | `Not selected by full` | `measured-and-passed` |
| `c-programs/ioctl-fioclex` | `verify` | `kvm` | `Selected by full` | `measured-and-passed` |
| `c-programs/ioctl-fioclex` | `verify` | `liteinst` | `Selected by full` | `measured-and-passed` |
| `c-programs/ioctl-fioclex` | `verify` | `ptrace` | `Selected by full` | `measured-and-passed` |
| `c-programs/ioctl-fioclex` | `verify` | `sabre` | `Selected by full` | `measured-and-passed` |
| `c-programs/ioctl-siocethtool` | `verify` | `kvm` | `Selected by full` | `measured-and-passed` |
| `c-programs/ioctl-siocethtool` | `verify` | `ptrace` | `Selected by full` | `measured-and-passed` |
| `c-programs/ioctl-siocethtool` | `verify` | `sabre` | `Selected by full` | `measured-and-passed` |
| `c-programs/ipc-determinism` | `chaos` | `ptrace` | `Selected by full` | `measured-and-passed` |
| `c-programs/ipc-determinism` | `verify` | `ptrace` | `Selected by full` | `measured-and-passed` |
| `c-programs/just-spin` | `verify` | `kvm` | `Not selected by full` | `diverged` |
| `c-programs/just-spin` | `verify` | `ptrace` | `Selected by full` | `measured-and-passed` |
| `c-programs/just-spin` | `verify` | `sabre` | `Not selected by full` | `measured-no-verdict` |
| `c-programs/kcmp-eperm` | `verify` | `dbt` | `Not selected by full` | `measured-and-passed` |
| `c-programs/kcmp-eperm` | `verify` | `kvm` | `Selected by full` | `measured-and-passed` |
| `c-programs/kcmp-eperm` | `verify` | `ptrace` | `Selected by full` | `measured-and-passed` |
| `c-programs/kcmp-eperm` | `verify` | `sabre` | `Selected by full` | `measured-and-passed` |
| `c-programs/keyctl-enosys` | `verify` | `dbt` | `Not selected by full` | `measured-and-passed` |
| `c-programs/keyctl-enosys` | `verify` | `kvm` | `Selected by full` | `measured-and-passed` |
| `c-programs/keyctl-enosys` | `verify` | `ptrace` | `Selected by full` | `measured-and-passed` |
| `c-programs/keyctl-enosys` | `verify` | `sabre` | `Selected by full` | `measured-and-passed` |
| `c-programs/keyctl-passthrough` | `verify` | `kvm` | `Selected by full` | `measured-and-passed` |
| `c-programs/keyctl-passthrough` | `verify` | `ptrace` | `Selected by full` | `measured-and-passed` |
| `c-programs/keyctl-passthrough` | `verify` | `sabre` | `Selected by full` | `diverged` |
| `c-programs/listmount-enosys` | `verify` | `dbt` | `Not selected by full` | `measured-and-passed` |
| `c-programs/listmount-enosys` | `verify` | `kvm` | `Selected by full` | `measured-and-passed` |
| `c-programs/listmount-enosys` | `verify` | `ptrace` | `Selected by full` | `measured-and-passed` |
| `c-programs/listmount-enosys` | `verify` | `sabre` | `Selected by full` | `measured-and-passed` |
| `c-programs/liteinst-advanced` | `verify` | `ptrace` | `Selected by full` | `measured-and-passed` |
| `c-programs/lsm-get-self-attr-enosys` | `verify` | `dbt` | `Not selected by full` | `measured-and-passed` |
| `c-programs/lsm-get-self-attr-enosys` | `verify` | `kvm` | `Selected by full` | `measured-and-passed` |
| `c-programs/lsm-get-self-attr-enosys` | `verify` | `ptrace` | `Selected by full` | `measured-and-passed` |
| `c-programs/lsm-get-self-attr-enosys` | `verify` | `sabre` | `Selected by full` | `measured-and-passed` |
| `c-programs/lsm-list-modules-enosys` | `verify` | `dbt` | `Not selected by full` | `measured-and-passed` |
| `c-programs/lsm-list-modules-enosys` | `verify` | `kvm` | `Selected by full` | `measured-and-passed` |
| `c-programs/lsm-list-modules-enosys` | `verify` | `ptrace` | `Selected by full` | `measured-and-passed` |
| `c-programs/lsm-list-modules-enosys` | `verify` | `sabre` | `Selected by full` | `measured-and-passed` |
| `c-programs/lsm-set-self-attr-enosys` | `verify` | `dbt` | `Not selected by full` | `measured-and-passed` |
| `c-programs/lsm-set-self-attr-enosys` | `verify` | `kvm` | `Selected by full` | `measured-and-passed` |
| `c-programs/lsm-set-self-attr-enosys` | `verify` | `ptrace` | `Selected by full` | `measured-and-passed` |
| `c-programs/lsm-set-self-attr-enosys` | `verify` | `sabre` | `Selected by full` | `measured-and-passed` |
| `c-programs/madvise-determinism` | `verify` | `kvm` | `Selected by full` | `measured-and-passed` |
| `c-programs/madvise-determinism` | `verify` | `ptrace` | `Selected by full` | `measured-and-passed` |
| `c-programs/madvise-determinism` | `verify` | `sabre` | `Selected by full` | `measured-and-passed` |
| `c-programs/map-shadow-stack-enosys` | `verify` | `dbt` | `Not selected by full` | `measured-and-passed` |
| `c-programs/map-shadow-stack-enosys` | `verify` | `kvm` | `Selected by full` | `measured-and-passed` |
| `c-programs/map-shadow-stack-enosys` | `verify` | `ptrace` | `Selected by full` | `measured-and-passed` |
| `c-programs/map-shadow-stack-enosys` | `verify` | `sabre` | `Selected by full` | `measured-and-passed` |
| `c-programs/memfd-secret-enosys` | `verify` | `dbt` | `Not selected by full` | `measured-and-passed` |
| `c-programs/memfd-secret-enosys` | `verify` | `kvm` | `Selected by full` | `measured-and-passed` |
| `c-programs/memfd-secret-enosys` | `verify` | `ptrace` | `Selected by full` | `measured-and-passed` |
| `c-programs/memfd-secret-enosys` | `verify` | `sabre` | `Selected by full` | `measured-and-passed` |
| `c-programs/meminfo-available-deterministic` | `verify` | `kvm` | `Selected by full` | `measured-and-passed` |
| `c-programs/meminfo-available-deterministic` | `verify` | `ptrace` | `Selected by full` | `measured-and-passed` |
| `c-programs/meminfo-available-deterministic` | `verify` | `sabre` | `Selected by full` | `measured-and-passed` |
| `c-programs/meminfo-cached-deterministic` | `verify` | `kvm` | `Selected by full` | `measured-and-passed` |
| `c-programs/meminfo-cached-deterministic` | `verify` | `ptrace` | `Selected by full` | `measured-and-passed` |
| `c-programs/meminfo-cached-deterministic` | `verify` | `sabre` | `Selected by full` | `measured-and-passed` |
| `c-programs/meminfo-free-deterministic` | `verify` | `kvm` | `Selected by full` | `measured-and-passed` |
| `c-programs/meminfo-free-deterministic` | `verify` | `ptrace` | `Selected by full` | `measured-and-passed` |
| `c-programs/meminfo-free-deterministic` | `verify` | `sabre` | `Selected by full` | `measured-and-passed` |
| `c-programs/memorypress` | `verify` | `kvm` | `Selected by full` | `measured-and-passed` |
| `c-programs/memorypress` | `verify` | `ptrace` | `Selected by full` | `measured-and-passed` |
| `c-programs/memorypress` | `verify` | `sabre` | `Selected by full` | `measured-and-passed` |
| `c-programs/mmap-determinism` | `verify` | `kvm` | `Selected by full` | `measured-and-passed` |
| `c-programs/mmap-determinism` | `verify` | `liteinst` | `Selected by full` | `measured-and-passed` |
| `c-programs/mmap-determinism` | `verify` | `ptrace` | `Selected by full` | `measured-and-passed` |
| `c-programs/mmap-determinism` | `verify` | `sabre` | `Selected by full` | `measured-and-passed` |
| `c-programs/mmap-stress-determinism` | `verify` | `kvm` | `Selected by full` | `measured-and-passed` |
| `c-programs/mmap-stress-determinism` | `verify` | `ptrace` | `Selected by full` | `measured-and-passed` |
| `c-programs/mmap-stress-determinism` | `verify` | `sabre` | `Selected by full` | `measured-and-passed` |
| `c-programs/name-to-handle-at-eopnotsupp` | `verify` | `kvm` | `Selected by full` | `measured-and-passed` |
| `c-programs/name-to-handle-at-eopnotsupp` | `verify` | `liteinst` | `Selected by full` | `measured-and-passed` |
| `c-programs/name-to-handle-at-eopnotsupp` | `verify` | `ptrace` | `Selected by full` | `measured-and-passed` |
| `c-programs/name-to-handle-at-eopnotsupp` | `verify` | `sabre` | `Selected by full` | `measured-and-passed` |
| `c-programs/name-to-handle-directory-eopnotsupp` | `verify` | `kvm` | `Selected by full` | `measured-and-passed` |
| `c-programs/name-to-handle-directory-eopnotsupp` | `verify` | `liteinst` | `Selected by full` | `measured-and-passed` |
| `c-programs/name-to-handle-directory-eopnotsupp` | `verify` | `ptrace` | `Selected by full` | `measured-and-passed` |
| `c-programs/name-to-handle-directory-eopnotsupp` | `verify` | `sabre` | `Selected by full` | `measured-and-passed` |
| `c-programs/name-to-handle-empty-path-eopnotsupp` | `verify` | `kvm` | `Selected by full` | `measured-and-passed` |
| `c-programs/name-to-handle-empty-path-eopnotsupp` | `verify` | `liteinst` | `Selected by full` | `measured-and-passed` |
| `c-programs/name-to-handle-empty-path-eopnotsupp` | `verify` | `ptrace` | `Selected by full` | `measured-and-passed` |
| `c-programs/name-to-handle-empty-path-eopnotsupp` | `verify` | `sabre` | `Selected by full` | `measured-and-passed` |
| `c-programs/name-to-handle-regular-eopnotsupp` | `verify` | `kvm` | `Selected by full` | `measured-and-passed` |
| `c-programs/name-to-handle-regular-eopnotsupp` | `verify` | `liteinst` | `Selected by full` | `measured-and-passed` |
| `c-programs/name-to-handle-regular-eopnotsupp` | `verify` | `ptrace` | `Selected by full` | `measured-and-passed` |
| `c-programs/name-to-handle-regular-eopnotsupp` | `verify` | `sabre` | `Selected by full` | `measured-and-passed` |
| `c-programs/nanosleep-par` | `verify` | `kvm` | `Selected by full` | `diverged` |
| `c-programs/nanosleep-par` | `verify` | `ptrace` | `Selected by full` | `measured-and-passed` |
| `c-programs/nanosleep-par` | `verify` | `sabre` | `Not selected by full` | `measured-and-passed` |
| `c-programs/nanosleep-threads-nocrash` | `verify` | `ptrace` | `Selected by full` | `measured-and-passed` |
| `c-programs/nanosleep-threads-nocrash` | `verify` | `sabre` | `Not selected by full` | `diverged` |
| `c-programs/netlink-autobind-generic` | `verify` | `kvm` | `Selected by full` | `measured-and-passed` |
| `c-programs/netlink-autobind-generic` | `verify` | `liteinst` | `Selected by full` | `measured-and-passed` |
| `c-programs/netlink-autobind-generic` | `verify` | `ptrace` | `Selected by full` | `measured-and-passed` |
| `c-programs/netlink-autobind-generic` | `verify` | `sabre` | `Selected by full` | `measured-and-passed` |
| `c-programs/netlink-autobind-route` | `verify` | `kvm` | `Selected by full` | `measured-and-passed` |
| `c-programs/netlink-autobind-route` | `verify` | `liteinst` | `Selected by full` | `measured-and-passed` |
| `c-programs/netlink-autobind-route` | `verify` | `ptrace` | `Selected by full` | `measured-and-passed` |
| `c-programs/netlink-autobind-route` | `verify` | `sabre` | `Selected by full` | `measured-and-passed` |
| `c-programs/netlink-autobind-usersock` | `verify` | `kvm` | `Selected by full` | `measured-and-passed` |
| `c-programs/netlink-autobind-usersock` | `verify` | `liteinst` | `Selected by full` | `measured-and-passed` |
| `c-programs/netlink-autobind-usersock` | `verify` | `ptrace` | `Selected by full` | `measured-and-passed` |
| `c-programs/netlink-autobind-usersock` | `verify` | `sabre` | `Selected by full` | `measured-and-passed` |
| `c-programs/netns-cookie-tcp4` | `verify` | `kvm` | `Selected by full` | `measured-and-passed` |
| `c-programs/netns-cookie-tcp4` | `verify` | `ptrace` | `Selected by full` | `measured-and-passed` |
| `c-programs/netns-cookie-tcp4` | `verify` | `sabre` | `Selected by full` | `measured-and-passed` |
| `c-programs/netns-cookie-tcp6` | `verify` | `kvm` | `Selected by full` | `measured-and-passed` |
| `c-programs/netns-cookie-tcp6` | `verify` | `ptrace` | `Selected by full` | `measured-and-passed` |
| `c-programs/netns-cookie-tcp6` | `verify` | `sabre` | `Selected by full` | `measured-and-passed` |
| `c-programs/netns-cookie-udp4` | `verify` | `kvm` | `Selected by full` | `measured-and-passed` |
| `c-programs/netns-cookie-udp4` | `verify` | `ptrace` | `Selected by full` | `measured-and-passed` |
| `c-programs/netns-cookie-udp4` | `verify` | `sabre` | `Selected by full` | `measured-and-passed` |
| `c-programs/pause-alarm-interrupt` | `verify` | `liteinst` | `Selected by full` | `measured-and-passed` |
| `c-programs/pause-alarm-interrupt` | `verify` | `ptrace` | `Selected by full` | `measured-and-passed` |
| `c-programs/perf-event-hardware-enosys` | `verify` | `dbt` | `Not selected by full` | `measured-and-passed` |
| `c-programs/perf-event-hardware-enosys` | `verify` | `kvm` | `Selected by full` | `measured-and-passed` |
| `c-programs/perf-event-hardware-enosys` | `verify` | `ptrace` | `Selected by full` | `measured-and-passed` |
| `c-programs/perf-event-hardware-enosys` | `verify` | `sabre` | `Selected by full` | `measured-and-passed` |
| `c-programs/perf-event-open-enosys` | `verify` | `dbt` | `Not selected by full` | `measured-and-passed` |
| `c-programs/perf-event-open-enosys` | `verify` | `kvm` | `Selected by full` | `measured-and-passed` |
| `c-programs/perf-event-open-enosys` | `verify` | `ptrace` | `Selected by full` | `measured-and-passed` |
| `c-programs/perf-event-open-enosys` | `verify` | `sabre` | `Selected by full` | `measured-and-passed` |
| `c-programs/perf-event-software-enosys` | `verify` | `dbt` | `Not selected by full` | `measured-and-passed` |
| `c-programs/perf-event-software-enosys` | `verify` | `kvm` | `Selected by full` | `measured-and-passed` |
| `c-programs/perf-event-software-enosys` | `verify` | `ptrace` | `Selected by full` | `measured-and-passed` |
| `c-programs/perf-event-software-enosys` | `verify` | `sabre` | `Selected by full` | `measured-and-passed` |
| `c-programs/perf-event-watchpoint-enosys` | `verify` | `dbt` | `Not selected by full` | `measured-and-passed` |
| `c-programs/perf-event-watchpoint-enosys` | `verify` | `kvm` | `Selected by full` | `measured-and-passed` |
| `c-programs/perf-event-watchpoint-enosys` | `verify` | `ptrace` | `Selected by full` | `measured-and-passed` |
| `c-programs/perf-event-watchpoint-enosys` | `verify` | `sabre` | `Selected by full` | `measured-and-passed` |
| `c-programs/periodic-setitimer-delivery` | `verify` | `ptrace` | `Selected by full` | `measured-and-passed` |
| `c-programs/periodic-setitimer-delivery` | `verify` | `sabre` | `Selected by full` | `measured-and-passed` |
| `c-programs/pidfd-open-self` | `verify` | `kvm` | `Selected by full` | `measured-and-passed` |
| `c-programs/pidfd-open-self` | `verify` | `ptrace` | `Selected by full` | `measured-and-passed` |
| `c-programs/pidfd-open-self` | `verify` | `sabre` | `Selected by full` | `measured-and-passed` |
| `c-programs/pidfd-poll-self` | `verify` | `kvm` | `Selected by full` | `measured-and-passed` |
| `c-programs/pidfd-poll-self` | `verify` | `ptrace` | `Selected by full` | `diverged-unlocated` |
| `c-programs/pidfd-poll-self` | `verify` | `sabre` | `Selected by full` | `measured-and-passed` |
| `c-programs/pidfd-waitid-child` | `verify` | `ptrace` | `Selected by full` | `measured-and-passed` |
| `c-programs/pidfd-waitid-child` | `verify` | `sabre` | `Not selected by full` | `measured-and-passed` |
| `c-programs/pipe2-errno-precedence` | `verify` | `dbt` | `Not selected by full` | `measured-and-passed` |
| `c-programs/pipe2-errno-precedence` | `verify` | `kvm` | `Selected by full` | `measured-and-passed` |
| `c-programs/pipe2-errno-precedence` | `verify` | `ptrace` | `Selected by full` | `measured-and-passed` |
| `c-programs/pipe2-errno-precedence` | `verify` | `sabre` | `Selected by full` | `measured-and-passed` |
| `c-programs/ppoll-readv` | `verify` | `kvm` | `Selected by full` | `measured-and-passed` |
| `c-programs/ppoll-readv` | `verify` | `ptrace` | `Selected by full` | `measured-and-passed` |
| `c-programs/ppoll-readv` | `verify` | `sabre` | `Selected by full` | `measured-and-passed` |
| `c-programs/ppoll-simulation` | `verify` | `ptrace` | `Selected by full` | `measured-and-passed` |
| `c-programs/ppoll-simulation` | `verify` | `sabre` | `Not selected by full` | `diverged` |
| `c-programs/prctl-dumpable` | `verify` | `kvm` | `Selected by full` | `measured-and-passed` |
| `c-programs/prctl-dumpable` | `verify` | `ptrace` | `Selected by full` | `measured-and-passed` |
| `c-programs/prctl-dumpable` | `verify` | `sabre` | `Selected by full` | `measured-and-passed` |
| `c-programs/prctl-option-policy` | `verify` | `kvm` | `Selected by full` | `measured-and-passed` |
| `c-programs/prctl-option-policy` | `verify` | `ptrace` | `Selected by full` | `measured-and-passed` |
| `c-programs/pread64-nostdlib` | `verify` | `kvm` | `Selected by full` | `measured-and-passed` |
| `c-programs/pread64-nostdlib` | `verify` | `ptrace` | `Selected by full` | `measured-and-passed` |
| `c-programs/print-memaddrs` | `verify` | `kvm` | `Selected by full` | `measured-and-passed` |
| `c-programs/print-memaddrs` | `verify` | `ptrace` | `Selected by full` | `measured-and-passed` |
| `c-programs/printf-with-threads` | `verify` | `ptrace` | `Selected by full` | `measured-and-passed` |
| `c-programs/printf-with-threads` | `verify` | `sabre` | `Not selected by full` | `diverged` |
| `c-programs/proc-fd-link-aliases` | `verify` | `liteinst` | `Selected by full` | `measured-and-passed` |
| `c-programs/proc-fd-link-aliases` | `verify` | `ptrace` | `Selected by full` | `measured-and-passed` |
| `c-programs/proc-fd-link-aliases` | `verify` | `sabre` | `Selected by full` | `measured-and-passed` |
| `c-programs/proc-fdinfo` | `verify` | `ptrace` | `Selected by full` | `measured-and-passed` |
| `c-programs/proc-locks` | `verify` | `kvm` | `Selected by full` | `measured-and-passed` |
| `c-programs/proc-locks` | `verify` | `ptrace` | `Selected by full` | `diverged` |
| `c-programs/proc-locks` | `verify` | `sabre` | `Selected by full` | `measured-and-passed` |
| `c-programs/process-mrelease-enosys` | `verify` | `dbt` | `Not selected by full` | `measured-and-passed` |
| `c-programs/process-mrelease-enosys` | `verify` | `kvm` | `Selected by full` | `measured-and-passed` |
| `c-programs/process-mrelease-enosys` | `verify` | `ptrace` | `Selected by full` | `measured-and-passed` |
| `c-programs/process-mrelease-enosys` | `verify` | `sabre` | `Selected by full` | `measured-and-passed` |
| `c-programs/process-vm-readv-refusal-probe` | `verify` | `dbt` | `Not selected by full` | `measured-and-passed` |
| `c-programs/process-vm-readv-refusal-probe` | `verify` | `kvm` | `Selected by full` | `measured-and-passed` |
| `c-programs/process-vm-readv-refusal-probe` | `verify` | `ptrace` | `Selected by full` | `measured-and-passed` |
| `c-programs/process-vm-readv-refusal-probe` | `verify` | `sabre` | `Selected by full` | `measured-and-passed` |
| `c-programs/process-vm-writev-refusal-probe` | `verify` | `dbt` | `Not selected by full` | `measured-and-passed` |
| `c-programs/process-vm-writev-refusal-probe` | `verify` | `kvm` | `Selected by full` | `measured-and-passed` |
| `c-programs/process-vm-writev-refusal-probe` | `verify` | `ptrace` | `Selected by full` | `measured-and-passed` |
| `c-programs/process-vm-writev-refusal-probe` | `verify` | `sabre` | `Selected by full` | `measured-and-passed` |
| `c-programs/procfs-identity-agreement` | `verify` | `ptrace` | `Selected by full` | `measured-and-passed` |
| `c-programs/procfs-identity-agreement` | `verify` | `sabre` | `Selected by full` | `measured-and-passed` |
| `c-programs/procfs-positioned-probe` | `verify` | `kvm` | `Selected by full` | `measured-and-passed` |
| `c-programs/procfs-positioned-probe` | `verify` | `ptrace` | `Selected by full` | `diverged-unlocated` |
| `c-programs/procfs-positioned-probe` | `verify` | `sabre` | `Selected by full` | `measured-and-passed` |
| `c-programs/prodcons-determinism` | `verify` | `kvm` | `Not selected by full` | `measured-and-passed` |
| `c-programs/prodcons-determinism` | `verify` | `ptrace` | `Selected by full` | `measured-and-passed` |
| `c-programs/prodcons-determinism` | `verify` | `sabre` | `Not selected by full` | `diverged` |
| `c-programs/pselect6-simulation` | `verify` | `ptrace` | `Selected by full` | `measured-and-passed` |
| `c-programs/ptrace-attach-eperm` | `verify` | `kvm` | `Selected by full` | `diverged` |
| `c-programs/ptrace-attach-eperm` | `verify` | `ptrace` | `Selected by full` | `measured-and-passed` |
| `c-programs/ptrace-attach-eperm` | `verify` | `sabre` | `Not selected by full` | `measured-and-passed` |
| `c-programs/ptrace-eperm` | `verify` | `dbt` | `Not selected by full` | `measured-and-passed` |
| `c-programs/ptrace-eperm` | `verify` | `kvm` | `Selected by full` | `measured-and-passed` |
| `c-programs/ptrace-eperm` | `verify` | `ptrace` | `Selected by full` | `measured-and-passed` |
| `c-programs/ptrace-eperm` | `verify` | `sabre` | `Selected by full` | `measured-and-passed` |
| `c-programs/ptrace-seize-eperm` | `verify` | `ptrace` | `Selected by full` | `measured-and-passed` |
| `c-programs/ptrace-seize-eperm` | `verify` | `sabre` | `Not selected by full` | `measured-and-passed` |
| `c-programs/ptrace-traceme-eperm` | `verify` | `dbt` | `Not selected by full` | `measured-and-passed` |
| `c-programs/ptrace-traceme-eperm` | `verify` | `kvm` | `Selected by full` | `measured-and-passed` |
| `c-programs/ptrace-traceme-eperm` | `verify` | `ptrace` | `Selected by full` | `measured-and-passed` |
| `c-programs/ptrace-traceme-eperm` | `verify` | `sabre` | `Selected by full` | `measured-and-passed` |
| `c-programs/pty-nr-count` | `verify` | `ptrace` | `Selected by full` | `measured-and-passed` |
| `c-programs/pty-nr-count` | `verify` | `sabre` | `Selected by full` | `measured-and-passed` |
| `c-programs/racewrite-nostdlib` | `verify` | `kvm` | `Selected by full` | `diverged` |
| `c-programs/racewrite-nostdlib` | `verify` | `ptrace` | `Selected by full` | `measured-and-passed` |
| `c-programs/random-sources` | `verify` | `ptrace` | `Selected by full` | `measured-and-passed` |
| `c-programs/rcx-canonicalization` | `verify` | `kvm` | `Selected by full` | `measured-and-passed` |
| `c-programs/rcx-canonicalization` | `verify` | `ptrace` | `Selected by full` | `measured-and-passed` |
| `c-programs/rcx-canonicalization` | `verify` | `sabre` | `Selected by full` | `measured-and-passed` |
| `c-programs/record-replay-fd-close` | `verify` | `kvm` | `Selected by full` | `measured-and-passed` |
| `c-programs/record-replay-fd-close` | `verify` | `ptrace` | `Selected by full` | `measured-and-passed` |
| `c-programs/record-replay-fd-close` | `verify` | `sabre` | `Not selected by full` | `measured-no-verdict` |
| `c-programs/record-replay-file-state` | `verify` | `ptrace` | `Not selected by full` | `diverged` |
| `c-programs/record-replay-lseek-seek-cur` | `verify` | `ptrace` | `Selected by full` | `measured-and-passed` |
| `c-programs/record-replay-lseek-seek-cur` | `verify` | `sabre` | `Selected by full` | `measured-and-passed` |
| `c-programs/record-replay-setsockopt` | `verify` | `kvm` | `Selected by full` | `measured-and-passed` |
| `c-programs/record-replay-setsockopt` | `verify` | `ptrace` | `Selected by full` | `measured-and-passed` |
| `c-programs/record-replay-setsockopt` | `verify` | `sabre` | `Selected by full` | `measured-and-passed` |
| `c-programs/recvmsg-scm-rights-mmap` | `verify` | `kvm` | `Selected by full` | `measured-and-passed` |
| `c-programs/recvmsg-scm-rights-mmap` | `verify` | `ptrace` | `Selected by full` | `measured-and-passed` |
| `c-programs/recvmsg-scm-rights-mmap` | `verify` | `sabre` | `Selected by full` | `measured-and-passed` |
| `c-programs/remap-file-pages-anonymous-enosys` | `verify` | `dbt` | `Not selected by full` | `measured-and-passed` |
| `c-programs/remap-file-pages-anonymous-enosys` | `verify` | `kvm` | `Selected by full` | `measured-and-passed` |
| `c-programs/remap-file-pages-anonymous-enosys` | `verify` | `ptrace` | `Selected by full` | `measured-and-passed` |
| `c-programs/remap-file-pages-anonymous-enosys` | `verify` | `sabre` | `Selected by full` | `measured-and-passed` |
| `c-programs/remap-file-pages-memfd-enosys` | `verify` | `dbt` | `Not selected by full` | `measured-and-passed` |
| `c-programs/remap-file-pages-memfd-enosys` | `verify` | `kvm` | `Selected by full` | `measured-and-passed` |
| `c-programs/remap-file-pages-memfd-enosys` | `verify` | `ptrace` | `Selected by full` | `measured-and-passed` |
| `c-programs/remap-file-pages-memfd-enosys` | `verify` | `sabre` | `Selected by full` | `measured-and-passed` |
| `c-programs/remap-file-pages-tmpfile-enosys` | `verify` | `dbt` | `Not selected by full` | `diverged` |
| `c-programs/remap-file-pages-tmpfile-enosys` | `verify` | `kvm` | `Selected by full` | `measured-and-passed` |
| `c-programs/remap-file-pages-tmpfile-enosys` | `verify` | `ptrace` | `Selected by full` | `measured-and-passed` |
| `c-programs/remap-file-pages-tmpfile-enosys` | `verify` | `sabre` | `Not selected by full` | `measured-and-passed` |
| `c-programs/request-key-enosys` | `verify` | `dbt` | `Not selected by full` | `measured-and-passed` |
| `c-programs/request-key-enosys` | `verify` | `kvm` | `Selected by full` | `measured-and-passed` |
| `c-programs/request-key-enosys` | `verify` | `ptrace` | `Selected by full` | `measured-and-passed` |
| `c-programs/request-key-enosys` | `verify` | `sabre` | `Selected by full` | `measured-and-passed` |
| `c-programs/sched-setattr-batch` | `verify` | `kvm` | `Selected by full` | `measured-and-passed` |
| `c-programs/sched-setattr-batch` | `verify` | `ptrace` | `Selected by full` | `measured-and-passed` |
| `c-programs/sched-setattr-batch` | `verify` | `sabre` | `Selected by full` | `diverged` |
| `c-programs/sched-setattr-idle` | `verify` | `kvm` | `Selected by full` | `measured-and-passed` |
| `c-programs/sched-setattr-idle` | `verify` | `ptrace` | `Selected by full` | `measured-and-passed` |
| `c-programs/sched-setattr-idle` | `verify` | `sabre` | `Selected by full` | `diverged` |
| `c-programs/sched-setattr-other` | `verify` | `kvm` | `Selected by full` | `measured-and-passed` |
| `c-programs/sched-setattr-other` | `verify` | `ptrace` | `Selected by full` | `measured-and-passed` |
| `c-programs/sched-setattr-other` | `verify` | `sabre` | `Selected by full` | `measured-and-passed` |
| `c-programs/sched-yield-progress` | `verify` | `ptrace` | `Selected by full` | `measured-and-passed` |
| `c-programs/sched-yield-progress` | `verify` | `sabre` | `Not selected by full` | `diverged` |
| `c-programs/scheduler-policy-queries` | `verify` | `kvm` | `Selected by full` | `measured-and-passed` |
| `c-programs/scheduler-policy-queries` | `verify` | `ptrace` | `Selected by full` | `measured-and-passed` |
| `c-programs/scheduler-policy-queries` | `verify` | `sabre` | `Selected by full` | `measured-and-passed` |
| `c-programs/session-identity` | `verify` | `kvm` | `Selected by full` | `measured-and-passed` |
| `c-programs/session-identity` | `verify` | `ptrace` | `Selected by full` | `measured-and-passed` |
| `c-programs/setitimer-determinism` | `verify` | `kvm` | `Selected by full` | `measured-and-passed` |
| `c-programs/setitimer-determinism` | `verify` | `ptrace` | `Selected by full` | `measured-and-passed` |
| `c-programs/sigmask-preemption` | `verify` | `ptrace` | `Selected by full` | `measured-and-passed` |
| `c-programs/sigmask-preemption` | `verify` | `sabre` | `Not selected by full` | `diverged` |
| `c-programs/signal-determinism` | `verify` | `ptrace` | `Selected by full` | `measured-and-passed` |
| `c-programs/sigpipe-siginfo` | `verify` | `ptrace` | `Selected by full` | `measured-and-passed` |
| `c-programs/sigtimedwait-no-timeout` | `verify` | `ptrace` | `Selected by full` | `measured-and-passed` |
| `c-programs/sigtimedwait-timeout-0s` | `verify` | `ptrace` | `Selected by full` | `measured-and-passed` |
| `c-programs/sigtimedwait-timeout-0s` | `verify` | `sabre` | `Not selected by full` | `measured-no-verdict` |
| `c-programs/sigtimedwait-timeout-1s` | `verify` | `ptrace` | `Selected by full` | `measured-and-passed` |
| `c-programs/so-incoming-cpu-tcp4` | `verify` | `kvm` | `Selected by full` | `measured-and-passed` |
| `c-programs/so-incoming-cpu-tcp4` | `verify` | `liteinst` | `Selected by full` | `measured-and-passed` |
| `c-programs/so-incoming-cpu-tcp4` | `verify` | `ptrace` | `Selected by full` | `measured-and-passed` |
| `c-programs/so-incoming-cpu-tcp4` | `verify` | `sabre` | `Not selected by full` | `measured-and-passed` |
| `c-programs/so-incoming-cpu-tcp6` | `verify` | `kvm` | `Selected by full` | `measured-and-passed` |
| `c-programs/so-incoming-cpu-tcp6` | `verify` | `liteinst` | `Selected by full` | `measured-and-passed` |
| `c-programs/so-incoming-cpu-tcp6` | `verify` | `ptrace` | `Selected by full` | `measured-and-passed` |
| `c-programs/so-incoming-cpu-tcp6` | `verify` | `sabre` | `Selected by full` | `measured-and-passed` |
| `c-programs/so-incoming-cpu-udp4` | `verify` | `kvm` | `Selected by full` | `measured-and-passed` |
| `c-programs/so-incoming-cpu-udp4` | `verify` | `liteinst` | `Selected by full` | `measured-and-passed` |
| `c-programs/so-incoming-cpu-udp4` | `verify` | `ptrace` | `Selected by full` | `measured-and-passed` |
| `c-programs/so-incoming-cpu-udp4` | `verify` | `sabre` | `Selected by full` | `measured-and-passed` |
| `c-programs/socket-cookie-tcp` | `verify` | `kvm` | `Selected by full` | `measured-and-passed` |
| `c-programs/socket-cookie-tcp` | `verify` | `liteinst` | `Selected by full` | `measured-and-passed` |
| `c-programs/socket-cookie-tcp` | `verify` | `ptrace` | `Selected by full` | `measured-and-passed` |
| `c-programs/socket-cookie-tcp` | `verify` | `sabre` | `Selected by full` | `measured-and-passed` |
| `c-programs/socket-cookie-udp` | `verify` | `kvm` | `Selected by full` | `measured-and-passed` |
| `c-programs/socket-cookie-udp` | `verify` | `liteinst` | `Selected by full` | `measured-and-passed` |
| `c-programs/socket-cookie-udp` | `verify` | `ptrace` | `Selected by full` | `measured-and-passed` |
| `c-programs/socket-cookie-udp` | `verify` | `sabre` | `Selected by full` | `measured-and-passed` |
| `c-programs/socket-cookie-unix` | `verify` | `kvm` | `Selected by full` | `measured-and-passed` |
| `c-programs/socket-cookie-unix` | `verify` | `liteinst` | `Selected by full` | `measured-and-passed` |
| `c-programs/socket-cookie-unix` | `verify` | `ptrace` | `Selected by full` | `measured-and-passed` |
| `c-programs/socket-cookie-unix` | `verify` | `sabre` | `Selected by full` | `measured-and-passed` |
| `c-programs/socket-ioctl-timestamp` | `verify` | `liteinst` | `Selected by full` | `measured-and-passed` |
| `c-programs/socket-ioctl-timestamp` | `verify` | `ptrace` | `Selected by full` | `measured-and-passed` |
| `c-programs/socket-timestamp-timespec` | `verify` | `kvm` | `Selected by full` | `measured-and-passed` |
| `c-programs/socket-timestamp-timespec` | `verify` | `liteinst` | `Selected by full` | `measured-and-passed` |
| `c-programs/socket-timestamp-timespec` | `verify` | `ptrace` | `Selected by full` | `measured-and-passed` |
| `c-programs/socket-timestamp-timeval` | `verify` | `kvm` | `Selected by full` | `measured-and-passed` |
| `c-programs/socket-timestamp-timeval` | `verify` | `liteinst` | `Selected by full` | `measured-and-passed` |
| `c-programs/socket-timestamp-timeval` | `verify` | `ptrace` | `Selected by full` | `measured-and-passed` |
| `c-programs/splice-enosys` | `verify` | `dbt` | `Not selected by full` | `measured-and-passed` |
| `c-programs/splice-enosys` | `verify` | `kvm` | `Selected by full` | `measured-and-passed` |
| `c-programs/splice-enosys` | `verify` | `ptrace` | `Selected by full` | `measured-and-passed` |
| `c-programs/splice-enosys` | `verify` | `sabre` | `Selected by full` | `measured-and-passed` |
| `c-programs/statmount-enosys` | `verify` | `dbt` | `Not selected by full` | `measured-and-passed` |
| `c-programs/statmount-enosys` | `verify` | `kvm` | `Selected by full` | `measured-and-passed` |
| `c-programs/statmount-enosys` | `verify` | `ptrace` | `Selected by full` | `measured-and-passed` |
| `c-programs/statmount-enosys` | `verify` | `sabre` | `Selected by full` | `measured-and-passed` |
| `c-programs/syscall-file-io` | `verify` | `kvm` | `Selected by full` | `measured-and-passed` |
| `c-programs/syscall-file-io` | `verify` | `ptrace` | `Selected by full` | `measured-and-passed` |
| `c-programs/syscall-file-io` | `verify` | `sabre` | `Selected by full` | `measured-and-passed` |
| `c-programs/syscall-file-metadata` | `verify` | `kvm` | `Not selected by full` | `measured-and-passed` |
| `c-programs/syscall-file-metadata` | `verify` | `ptrace` | `Selected by full` | `measured-and-passed` |
| `c-programs/syscall-file-metadata` | `verify` | `sabre` | `Selected by full` | `measured-and-passed` |
| `c-programs/syscall-quick-wins` | `verify` | `kvm` | `Selected by full` | `measured-and-passed` |
| `c-programs/syscall-quick-wins` | `verify` | `ptrace` | `Selected by full` | `measured-and-passed` |
| `c-programs/syscall-quick-wins` | `verify` | `sabre` | `Selected by full` | `measured-and-passed` |
| `c-programs/sysfs-enosys` | `verify` | `dbt` | `Not selected by full` | `measured-and-passed` |
| `c-programs/sysfs-enosys` | `verify` | `kvm` | `Selected by full` | `measured-and-passed` |
| `c-programs/sysfs-enosys` | `verify` | `ptrace` | `Selected by full` | `measured-and-passed` |
| `c-programs/sysfs-enosys` | `verify` | `sabre` | `Selected by full` | `measured-and-passed` |
| `c-programs/sysinfo` | `verify` | `ptrace` | `Selected by full` | `measured-and-passed` |
| `c-programs/sysinfo-uptime` | `verify` | `ptrace` | `Selected by full` | `measured-and-passed` |
| `c-programs/syslog-deterministic` | `verify` | `dbt` | `Not selected by full` | `measured-and-passed` |
| `c-programs/syslog-deterministic` | `verify` | `kvm` | `Selected by full` | `measured-and-passed` |
| `c-programs/syslog-deterministic` | `verify` | `ptrace` | `Selected by full` | `measured-and-passed` |
| `c-programs/syslog-deterministic` | `verify` | `sabre` | `Selected by full` | `measured-and-passed` |
| `c-programs/sysv-sem-enosys` | `verify` | `dbt` | `Not selected by full` | `measured-and-passed` |
| `c-programs/sysv-sem-enosys` | `verify` | `kvm` | `Selected by full` | `measured-and-passed` |
| `c-programs/sysv-sem-enosys` | `verify` | `ptrace` | `Selected by full` | `measured-and-passed` |
| `c-programs/sysv-sem-enosys` | `verify` | `sabre` | `Selected by full` | `measured-and-passed` |
| `c-programs/sysv-shm-enosys` | `verify` | `dbt` | `Not selected by full` | `measured-and-passed` |
| `c-programs/sysv-shm-enosys` | `verify` | `kvm` | `Selected by full` | `measured-and-passed` |
| `c-programs/sysv-shm-enosys` | `verify` | `ptrace` | `Selected by full` | `measured-and-passed` |
| `c-programs/sysv-shm-enosys` | `verify` | `sabre` | `Selected by full` | `measured-and-passed` |
| `c-programs/tcp-info-accept4` | `verify` | `kvm` | `Selected by full` | `measured-and-passed` |
| `c-programs/tcp-info-accept4` | `verify` | `ptrace` | `Selected by full` | `measured-and-passed` |
| `c-programs/tcp-info-accept4` | `verify` | `sabre` | `Selected by full` | `measured-and-passed` |
| `c-programs/tcp-info-accept6` | `verify` | `kvm` | `Selected by full` | `measured-and-passed` |
| `c-programs/tcp-info-accept6` | `verify` | `ptrace` | `Selected by full` | `measured-and-passed` |
| `c-programs/tcp-info-accept6` | `verify` | `sabre` | `Selected by full` | `measured-and-passed` |
| `c-programs/tcp-info-client4` | `verify` | `kvm` | `Selected by full` | `measured-and-passed` |
| `c-programs/tcp-info-client4` | `verify` | `ptrace` | `Selected by full` | `measured-and-passed` |
| `c-programs/tcp-info-client4` | `verify` | `sabre` | `Selected by full` | `measured-and-passed` |
| `c-programs/tee-enosys` | `verify` | `dbt` | `Not selected by full` | `measured-and-passed` |
| `c-programs/tee-enosys` | `verify` | `kvm` | `Selected by full` | `measured-and-passed` |
| `c-programs/tee-enosys` | `verify` | `ptrace` | `Selected by full` | `measured-and-passed` |
| `c-programs/tee-enosys` | `verify` | `sabre` | `Selected by full` | `measured-and-passed` |
| `c-programs/thread-self-procfs-handoff` | `verify` | `ptrace` | `Selected by full` | `measured-and-passed` |
| `c-programs/thread-self-procfs-handoff` | `verify` | `sabre` | `Not selected by full` | `diverged` |
| `c-programs/thread-sync-determinism` | `chaos` | `ptrace` | `Selected by full` | `measured-and-passed` |
| `c-programs/thread-sync-determinism` | `verify` | `kvm` | `Not selected by full` | `diverged` |
| `c-programs/thread-sync-determinism` | `verify` | `ptrace` | `Selected by full` | `measured-and-passed` |
| `c-programs/threadexhaustion` | `verify` | `ptrace` | `Selected by full` | `measured-and-passed` |
| `c-programs/threadexhaustion` | `verify` | `sabre` | `Not selected by full` | `diverged` |
| `c-programs/timer-create-determinism` | `verify` | `kvm` | `Selected by full` | `measured-and-passed` |
| `c-programs/timer-create-determinism` | `verify` | `ptrace` | `Selected by full` | `measured-and-passed` |
| `c-programs/timer-create-determinism` | `verify` | `sabre` | `Selected by full` | `measured-and-passed` |
| `c-programs/uname` | `verify` | `kvm` | `Selected by full` | `measured-and-passed` |
| `c-programs/uname` | `verify` | `ptrace` | `Selected by full` | `measured-and-passed` |
| `c-programs/uname` | `verify` | `sabre` | `Selected by full` | `measured-and-passed` |
| `c-programs/unix-autobind-dgram` | `verify` | `kvm` | `Selected by full` | `measured-and-passed` |
| `c-programs/unix-autobind-dgram` | `verify` | `liteinst` | `Selected by full` | `measured-and-passed` |
| `c-programs/unix-autobind-dgram` | `verify` | `ptrace` | `Selected by full` | `measured-and-passed` |
| `c-programs/unix-autobind-dgram` | `verify` | `sabre` | `Selected by full` | `measured-and-passed` |
| `c-programs/unix-autobind-seqpacket` | `verify` | `kvm` | `Selected by full` | `measured-and-passed` |
| `c-programs/unix-autobind-seqpacket` | `verify` | `liteinst` | `Selected by full` | `measured-and-passed` |
| `c-programs/unix-autobind-seqpacket` | `verify` | `ptrace` | `Selected by full` | `measured-and-passed` |
| `c-programs/unix-autobind-seqpacket` | `verify` | `sabre` | `Selected by full` | `measured-and-passed` |
| `c-programs/unix-autobind-stream` | `verify` | `kvm` | `Selected by full` | `measured-and-passed` |
| `c-programs/unix-autobind-stream` | `verify` | `liteinst` | `Selected by full` | `measured-and-passed` |
| `c-programs/unix-autobind-stream` | `verify` | `ptrace` | `Selected by full` | `measured-and-passed` |
| `c-programs/unix-autobind-stream` | `verify` | `sabre` | `Selected by full` | `measured-and-passed` |
| `c-programs/ustat-enosys` | `verify` | `dbt` | `Not selected by full` | `measured-and-passed` |
| `c-programs/ustat-enosys` | `verify` | `kvm` | `Selected by full` | `measured-and-passed` |
| `c-programs/ustat-enosys` | `verify` | `ptrace` | `Selected by full` | `measured-and-passed` |
| `c-programs/ustat-enosys` | `verify` | `sabre` | `Selected by full` | `measured-and-passed` |
| `c-programs/vforkexec` | `verify` | `kvm` | `Selected by full` | `measured-and-passed` |
| `c-programs/vforkexec` | `verify` | `ptrace` | `Selected by full` | `measured-and-passed` |
| `c-programs/vmsplice-enosys` | `verify` | `dbt` | `Not selected by full` | `measured-and-passed` |
| `c-programs/vmsplice-enosys` | `verify` | `kvm` | `Selected by full` | `measured-and-passed` |
| `c-programs/vmsplice-enosys` | `verify` | `ptrace` | `Selected by full` | `measured-and-passed` |
| `c-programs/vmsplice-enosys` | `verify` | `sabre` | `Selected by full` | `measured-and-passed` |
| `c-programs/wait-on-child` | `verify` | `kvm` | `Selected by full` | `diverged` |
| `c-programs/wait-on-child` | `verify` | `ptrace` | `Selected by full` | `measured-and-passed` |
| `c-programs/writev-determinism` | `verify` | `ptrace` | `Selected by full` | `measured-and-passed` |
| `chaos-c/lock-granularity` | `chaos` | `ptrace` | `Selected by full` | `measured-and-passed` |
| `chaos-c/lock-granularity` | `verify` | `ptrace` | `Selected by full` | `measured-and-passed` |
| `chaos-c/lock-granularity` | `verify` | `sabre` | `Not selected by full` | `diverged` |
| `data-handling/archive-roundtrip` | `verify` | `ptrace` | `Selected by full` | `measured-and-passed` |
| `data-handling/dd-partial-transfers` | `verify` | `ptrace` | `Selected by full` | `measured-and-passed` |
| `data-handling/jq-json-transform` | `verify` | `ptrace` | `Selected by full` | `measured-and-passed` |
| `data-handling/shell-pipeline` | `verify` | `ptrace` | `Selected by full` | `measured-and-passed` |
| `data-handling/sqlite-query-determinism` | `verify` | `ptrace` | `Selected by full` | `measured-and-passed` |
| `data-handling/sqlite-query-determinism` | `verify` | `sabre` | `Not applicable` | `diverged` |
| `data-handling/zstd-multithread` | `verify` | `ptrace` | `Selected by full` | `measured-and-passed` |
| `debugger-c/debuggee` | `verify` | `kvm` | `Selected by full` | `measured-and-passed` |
| `debugger-c/debuggee` | `verify` | `liteinst` | `Selected by full` | `measured-and-passed` |
| `debugger-c/debuggee` | `verify` | `ptrace` | `Selected by full` | `measured-and-passed` |
| `debugger-c/debuggee` | `verify` | `sabre` | `Selected by full` | `measured-and-passed` |
| `determinism-stress/example-race` | `verify` | `kvm` | `Selected by full` | `diverged` |
| `determinism-stress/example-race` | `verify` | `ptrace` | `Selected by full` | `measured-and-passed` |
| `determinism-stress/example-race` | `verify` | `sabre` | `Not selected by full` | `measured-no-verdict` |
| `determinism-stress/order-violation` | `chaos` | `ptrace` | `Not selected by full` | `measured-and-passed` |
| `determinism-stress/order-violation` | `verify` | `ptrace` | `Selected by full` | `measured-and-passed` |
| `determinism-stress/process-chains` | `verify` | `ptrace` | `Selected by full` | `measured-and-passed` |
| `determinism-stress/thread-contention` | `verify` | `ptrace` | `Selected by full` | `measured-and-passed` |
| `determinism-stress/thread-interleaving` | `chaos` | `ptrace` | `Selected by full` | `measured-and-passed` |
| `determinism-stress/thread-output` | `verify` | `ptrace` | `Selected by full` | `measured-and-passed` |
| `determinism-stress-c/fork-tree` | `verify` | `ptrace` | `Selected by full` | `measured-and-passed` |
| `determinism-stress-c/fork-tree` | `verify` | `sabre` | `Not selected by full` | `diverged` |
| `determinism-stress-c/lock-free` | `verify` | `ptrace` | `Selected by full` | `measured-and-passed` |
| `determinism-stress-c/mmap-fork-shared` | `verify` | `ptrace` | `Selected by full` | `measured-and-passed` |
| `determinism-stress-c/mmap-fork-shared` | `verify` | `sabre` | `Not selected by full` | `diverged` |
| `determinism-stress-c/pid-tid` | `verify` | `ptrace` | `Selected by full` | `measured-and-passed` |
| `determinism-stress-c/pid-tid-identity` | `verify` | `ptrace` | `Selected by full` | `measured-and-passed` |
| `determinism-stress-c/pipe-chain` | `verify` | `ptrace` | `Selected by full` | `measured-and-passed` |
| `determinism-stress-c/pipe-chain` | `verify` | `sabre` | `Not selected by full` | `diverged` |
| `determinism-stress-c/pipe-prefill` | `verify` | `ptrace` | `Selected by full` | `measured-and-passed` |
| `determinism-stress-c/producer-consumer` | `verify` | `ptrace` | `Selected by full` | `measured-and-passed` |
| `determinism-stress-c/signal-order` | `verify` | `ptrace` | `Selected by full` | `measured-and-passed` |
| `determinism-stress-c/thread-contention` | `chaos` | `ptrace` | `Selected by full` | `measured-and-passed` |
| `determinism-stress-c/thread-contention` | `verify` | `ptrace` | `Selected by full` | `measured-and-passed` |
| `determinism-stress-c/thread-stress` | `verify` | `ptrace` | `Selected by full` | `measured-and-passed` |
| `determinism-stress-c/thread-stress` | `verify` | `sabre` | `Not selected by full` | `measured-no-verdict` |
| `language-runtimes/bash-loop-pipe-time` | `verify` | `ptrace` | `Selected by full` | `measured-and-passed` |
| `language-runtimes/bash-random` | `verify` | `kvm` | `Selected by full` | `measured-and-passed` |
| `language-runtimes/bash-random` | `verify` | `ptrace` | `Selected by full` | `measured-and-passed` |
| `language-runtimes/cpp-stl-determinism` | `verify` | `kvm` | `Selected by full` | `measured-and-passed` |
| `language-runtimes/cpp-stl-determinism` | `verify` | `ptrace` | `Selected by full` | `measured-and-passed` |
| `language-runtimes/example-python-random` | `verify` | `dbt` | `Not selected by full` | `diverged` |
| `language-runtimes/example-python-random` | `verify` | `kvm` | `Selected by full` | `measured-and-passed` |
| `language-runtimes/example-python-random` | `verify` | `liteinst` | `Selected by full` | `measured-and-passed` |
| `language-runtimes/example-python-random` | `verify` | `ptrace` | `Selected by full` | `measured-and-passed` |
| `language-runtimes/example-python-random` | `verify` | `sabre` | `Not selected by full` | `diverged` |
| `language-runtimes/gawk-random` | `verify` | `kvm` | `Selected by full` | `measured-and-passed` |
| `language-runtimes/gawk-random` | `verify` | `ptrace` | `Selected by full` | `measured-and-passed` |
| `language-runtimes/lua-random` | `verify` | `ptrace` | `Selected by full` | `measured-and-passed` |
| `language-runtimes/m4-macro-mkstemp` | `verify` | `ptrace` | `Selected by full` | `measured-and-passed` |
| `language-runtimes/node-v8-jit` | `verify` | `ptrace` | `Selected by full` | `measured-and-passed` |
| `language-runtimes/perl-hash-order` | `verify` | `kvm` | `Selected by full` | `measured-and-passed` |
| `language-runtimes/perl-hash-order` | `verify` | `ptrace` | `Selected by full` | `measured-and-passed` |
| `language-runtimes/perl-io-subprocess-time` | `verify` | `kvm` | `Selected by full` | `diverged` |
| `language-runtimes/perl-io-subprocess-time` | `verify` | `ptrace` | `Selected by full` | `measured-and-passed` |
| `language-runtimes/perl-random` | `verify` | `kvm` | `Selected by full` | `measured-and-passed` |
| `language-runtimes/perl-random` | `verify` | `ptrace` | `Selected by full` | `measured-and-passed` |
| `language-runtimes/python-dict-hash-iteration` | `verify` | `ptrace` | `Selected by full` | `diverged-unlocated` |
| `language-runtimes/python-hash-determinism` | `verify` | `kvm` | `Selected by full` | `measured-and-passed` |
| `language-runtimes/python-hash-determinism` | `verify` | `ptrace` | `Selected by full` | `measured-and-passed` |
| `language-runtimes/python-hashseed` | `verify` | `ptrace` | `Not selected by full` | `diverged` |
| `language-runtimes/python-io-subprocess-time` | `verify` | `ptrace` | `Selected by full` | `diverged-unlocated` |
| `language-runtimes/python-random` | `verify` | `kvm` | `Selected by full` | `measured-and-passed` |
| `language-runtimes/python-random` | `verify` | `ptrace` | `Selected by full` | `measured-and-passed` |
| `language-runtimes/ruby-random` | `verify` | `ptrace` | `Selected by full` | `measured-and-passed` |
| `language-runtimes/rust-hashmap-iteration` | `verify` | `kvm` | `Selected by full` | `measured-and-passed` |
| `language-runtimes/rust-hashmap-iteration` | `verify` | `ptrace` | `Selected by full` | `measured-and-passed` |
| `language-runtimes/tcl-rand-clock` | `verify` | `ptrace` | `Selected by full` | `measured-and-passed` |
| `system-utils/auxv-loader-dump` | `verify` | `ptrace` | `Selected by full` | `measured-and-passed` |
| `system-utils/clock-determinism` | `verify` | `kvm` | `Selected by full` | `measured-and-passed` |
| `system-utils/clock-determinism` | `verify` | `liteinst` | `Selected by full` | `measured-and-passed` |
| `system-utils/clock-determinism` | `verify` | `ptrace` | `Selected by full` | `measured-and-passed` |
| `system-utils/clock-exec-continuity` | `verify` | `ptrace` | `Selected by full` | `measured-and-passed` |
| `system-utils/date-nanoseconds` | `verify` | `ptrace` | `Selected by full` | `measured-and-passed` |
| `system-utils/du-tree-summary` | `verify` | `kvm` | `Selected by full` | `measured-and-passed` |
| `system-utils/du-tree-summary` | `verify` | `ptrace` | `Selected by full` | `measured-and-passed` |
| `system-utils/errno-path-identity` | `verify` | `kvm` | `Selected by full` | `measured-and-passed` |
| `system-utils/errno-path-identity` | `verify` | `ptrace` | `Selected by full` | `measured-and-passed` |
| `system-utils/example-date` | `verify` | `kvm` | `Selected by full` | `measured-and-passed` |
| `system-utils/example-date` | `verify` | `ptrace` | `Selected by full` | `measured-and-passed` |
| `system-utils/example-devrand` | `verify` | `kvm` | `Selected by full` | `measured-and-passed` |
| `system-utils/example-devrand` | `verify` | `ptrace` | `Selected by full` | `measured-and-passed` |
| `system-utils/example-devrand` | `verify` | `sabre` | `Not selected by full` | `measured-no-verdict` |
| `system-utils/file-timestamp-identity` | `verify` | `kvm` | `Selected by full` | `measured-and-passed` |
| `system-utils/file-timestamp-identity` | `verify` | `ptrace` | `Selected by full` | `measured-and-passed` |
| `system-utils/find-tree-metadata` | `verify` | `kvm` | `Selected by full` | `measured-and-passed` |
| `system-utils/find-tree-metadata` | `verify` | `ptrace` | `Selected by full` | `measured-and-passed` |
| `system-utils/harness-width-contract` | `verify` | `kvm` | `Selected by full` | `measured-and-passed` |
| `system-utils/harness-width-contract` | `verify` | `ptrace` | `Selected by full` | `measured-and-passed` |
| `system-utils/mcookie-random` | `verify` | `kvm` | `Selected by full` | `measured-and-passed` |
| `system-utils/mcookie-random` | `verify` | `ptrace` | `Selected by full` | `measured-and-passed` |
| `system-utils/mktemp-name` | `verify` | `kvm` | `Selected by full` | `measured-and-passed` |
| `system-utils/mktemp-name` | `verify` | `ptrace` | `Selected by full` | `measured-and-passed` |
| `system-utils/nscd-neutralised` | `verify` | `ptrace` | `Selected by full` | `measured-and-passed` |
| `system-utils/openssl-enc` | `verify` | `ptrace` | `Selected by full` | `measured-and-passed` |
| `system-utils/openssl-genpkey` | `verify` | `kvm` | `Selected by full` | `measured-and-passed` |
| `system-utils/openssl-genpkey` | `verify` | `ptrace` | `Selected by full` | `measured-and-passed` |
| `system-utils/openssl-passwd` | `verify` | `kvm` | `Selected by full` | `measured-and-passed` |
| `system-utils/openssl-passwd` | `verify` | `ptrace` | `Selected by full` | `measured-and-passed` |
| `system-utils/openssl-rand` | `verify` | `kvm` | `Selected by full` | `measured-and-passed` |
| `system-utils/openssl-rand` | `verify` | `ptrace` | `Selected by full` | `measured-and-passed` |
| `system-utils/openssl-x509` | `verify` | `kvm` | `Selected by full` | `measured-and-passed` |
| `system-utils/openssl-x509` | `verify` | `ptrace` | `Selected by full` | `measured-and-passed` |
| `system-utils/overflow-gid-resolves` | `verify` | `ptrace` | `Selected by full` | `measured-and-passed` |
| `system-utils/proc-random-uuid` | `verify` | `ptrace` | `Selected by full` | `measured-and-passed` |
| `system-utils/proc-uptime` | `verify` | `kvm` | `Selected by full` | `measured-and-passed` |
| `system-utils/proc-uptime` | `verify` | `ptrace` | `Selected by full` | `measured-and-passed` |
| `system-utils/procfs-sanitized-paths` | `verify` | `ptrace` | `Not selected by full` | `diverged` |
| `system-utils/ps-proc-table` | `verify` | `ptrace` | `Selected by full` | `measured-and-passed` |
| `system-utils/random-device` | `verify` | `kvm` | `Selected by full` | `measured-and-passed` |
| `system-utils/random-device` | `verify` | `ptrace` | `Selected by full` | `measured-and-passed` |
| `system-utils/record-getpid` | `replay` | `ptrace` | `Selected by full` | `measured-and-passed` |
| `system-utils/record-getpid` | `verify` | `kvm` | `Selected by full` | `measured-and-passed` |
| `system-utils/record-getpid` | `verify` | `liteinst` | `Selected by full` | `measured-and-passed` |
| `system-utils/record-getpid` | `verify` | `ptrace` | `Selected by full` | `measured-and-passed` |
| `system-utils/record-getpid` | `verify` | `sabre` | `Selected by full` | `diverged` |
| `system-utils/shm-coherency-identity` | `verify` | `ptrace` | `Selected by full` | `measured-and-passed` |
| `system-utils/shuf-permutation` | `verify` | `kvm` | `Selected by full` | `measured-and-passed` |
| `system-utils/shuf-permutation` | `verify` | `ptrace` | `Selected by full` | `measured-and-passed` |
| `system-utils/sort-random` | `verify` | `ptrace` | `Selected by full` | `measured-and-passed` |
| `system-utils/ssh-keygen-ed25519` | `verify` | `ptrace` | `Selected by full` | `measured-and-passed` |
| `system-utils/startup-surface-identity` | `verify` | `kvm` | `Selected by full` | `measured-and-passed` |
| `system-utils/startup-surface-identity` | `verify` | `ptrace` | `Selected by full` | `measured-and-passed` |
| `system-utils/startup-tls-guards` | `verify` | `kvm` | `Selected by full` | `measured-and-passed` |
| `system-utils/startup-tls-guards` | `verify` | `ptrace` | `Selected by full` | `measured-and-passed` |
| `system-utils/startup-tls-guards` | `verify` | `sabre` | `Not selected by full` | `diverged` |
| `system-utils/sysfs-sanitized-prefixes` | `verify` | `kvm` | `Selected by full` | `measured-and-passed` |
| `system-utils/sysfs-sanitized-prefixes` | `verify` | `ptrace` | `Selected by full` | `measured-and-passed` |
| `system-utils/uuidgen-random` | `verify` | `kvm` | `Selected by full` | `measured-and-passed` |
| `system-utils/uuidgen-random` | `verify` | `ptrace` | `Selected by full` | `measured-and-passed` |
| `applications/kvm-python-examples` | `verify` | `kvm` | `Not selected by full` | `diverged` |
| `applications/kvm-shell-environment` | `verify` | `kvm` | `Selected by full` | `measured-and-passed` |
| `backend-parity-c/cpuid-probe` | `verify` | `kvm` | `Selected by full` | `measured-and-passed` |
| `backend-parity-c/cpuid-probe` | `verify` | `liteinst` | `Selected by full` | `measured-and-passed` |
| `backend-parity-c/cpuid-probe` | `verify` | `ptrace` | `Selected by full` | `measured-and-passed` |
