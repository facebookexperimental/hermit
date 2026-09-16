# LiteInst qualification: 22 verify cells, 2026-09-16

This change selects 22 previously enabled, unselected LiteInst `verify` cells
for ordinary validation in [the existing promotion PR](https://github.com/rrnewton/hermit/pull/2948).
Each passed ten clean first attempts in the official pressure runner on Hermit
`ed99e05133b00058fafed7f3dfaf48ab8fd334e6`, with strict canonical verification
and retained comparison logs. No runtime code, comparator, retry allowance or
timeout is changed.

Selected LiteInst verify coverage increases from **28 to 50**, compared with
341 selected ptrace verify cells. The newly measured population is only these
22 cells: this campaign does not freshly qualify all 50. Three enabled LiteInst
verify cells remain unselected, and 307 remain disabled at this point. The
separate 212-cell disabled campaign and all other gaps remain outside this
qualification. These counts describe selection, not a backend determinism or
cross-backend parity percentage.

## Evidence and execution scope

The completed `red22` run used `--cells-file old-promotions-red.jsonl
--repetitions 10 --jobs 8 --manifest-guest-cap 8 --run-timeout 21600`, with
`CARGO_BUILD_JOBS=8` and `HERMIT_LOG_MAX_BYTES=1073741824`. Its exact cells-file
SHA-256 is `7c3b4c85cda4a6c455761b994867bb3557af42de7da471fee22d50a0ff4c2ccf`;
the sorted full-population SHA-256 is
`f432e376793358cfdfa67ee34974d8b910a727e94f7656a3e43bbcb4d132065e`.

The supervisor recorded 2026-09-16 22:04:58.441195 through 22:31:37.959268 UTC:
**1599.518 seconds**, including all build and setup producers. All 253 DAG nodes
succeeded: ten producers, 22 fixture preparations, 220 repetitions and the
summary. Every repetition has exactly one outer attempt and one passing inner
verification, with zero retries, missing repetitions, prerequisite failures,
infrastructure failures, product failures or no-result rows. These are the
existing `promotion-candidate` requirements for ten repetitions; no obsolete
three-repetition policy is used.

The 220 reports declare the complete `BitwiseInfoV1` policy and matched verdicts,
positive equal INFO counts, no relaxation, and byte comparisons of exit status,
stdout and stderr. They compare two executions of the **LiteInst host hybrid**:
the activated LiteInst patch runtime with the ptrace Detcore Tool. This is
same-backend strict repeat verification, not ptrace-free/in-process LiteInst or
a comparison against a ptrace golden run. The test category `backend-parity-c`
does not establish cross-backend parity.

The source binding is Detcore tree
`779213274890cd37daafae32ea5a73c1f84090a6`, Reverie
`a158914eceeca02a9ab4c7dd4e9916926d5e5c1e`, and LiteInst2
`95ee5e6917fa33191eb41c3f1606ea8b03c1b78c`. The actual result records name Hermit
binary SHA-256 `a3d3c8651c1b28c437da277693aad317b6cba3f5e685059e57ddab6958a4f6c7`
and complete E2E artifact identity
`eb1f56d7e0d50999fcfb8ed6954133527a8bd2f8e9fcd1cf9691e7daa6947263`.
Staging completed and every invocation passed the existing activation check
before running its verification pair. The official runner removed its disposable
checkout afterward, so the executed ELF and installed bundle are **recorded
execution identities, not independently rehashed retained binaries**. No claim
about every workload's patch coverage follows from the activation probe.

An independent audit by `/root/resume_slot_diagnosis` read all 220 raw rows and
reports and all 440 retained log files. It verified 267,770 INFO records per
side, complete matching canonical pairs, no terminal truncation marker, and
all files contained within the campaign quota. Those logs total 119,754,860
logical bytes; the largest is 458,443 bytes. The 1 GiB logical per-log cap and
64 GiB compressed on-disk aggregate quota remained enabled. The audit approved
these 22 current promotion candidates with the runtime and missing-binary
limits above.

## Exact population and timeout calibration

Every row below has `lane=portable`, `mode=verify`, `backend=liteinst`; its
category is the prefix before `/`. Each has ten first-attempt result rows.
CPU is the row's aggregate `cpu_usage_usec`, wall is `duration_ms`, including
reported preparation, rather than only the inner invocation duration. The
nearest-rank p90 is the ninth sorted value of ten, calculated independently
for CPU and wall. The existing owner formula is `ceil(1.5 × p90 CPU)` and
`ceil(4 × p90 wall)`, or `ceil(3 × p90 wall)` if the fourfold value exceeds
120 seconds. The units below are explicit.

| Test | p90 CPU (µs) | p90 wall (ms) | Derived CPU/wall (s) |
| --- | ---: | ---: | ---: |
| backend-parity-c/aio-refusal | 1,662,030 | 3,013 | 3/13 |
| backend-parity-c/cwd-roundtrip | 1,782,568 | 3,048 | 3/13 |
| backend-parity-c/event-delivery-ordering | 1,757,211 | 3,174 | 3/13 |
| backend-parity-c/eventfd-semantics | 1,810,155 | 3,137 | 3/13 |
| backend-parity-c/fcntl-owner | 1,720,047 | 3,170 | 3/13 |
| backend-parity-c/file-io-roundtrip | 1,773,049 | 3,126 | 3/13 |
| backend-parity-c/membarrier-query | 1,759,121 | 3,083 | 3/13 |
| backend-parity-c/mkdir-rmdir | 1,726,751 | 3,025 | 3/13 |
| backend-parity-c/o-tmpfile-anon | 1,836,947 | 3,233 | 3/13 |
| backend-parity-c/personality-domain | 1,692,162 | 3,048 | 3/13 |
| backend-parity-c/pipe-capacity | 1,800,792 | 3,190 | 3/13 |
| backend-parity-c/record-lock | 1,788,952 | 3,126 | 3/13 |
| backend-parity-c/sendfile-copy | 1,683,977 | 3,031 | 3/13 |
| backend-parity-c/set-tid-address | 1,647,848 | 2,982 | 3/12 |
| backend-parity-c/signal-delivery-sequence | 2,136,272 | 3,621 | 4/15 |
| backend-parity-c/symlink-ops | 1,752,774 | 3,212 | 3/13 |
| backend-parity-c/umask-mode | 2,053,977 | 3,551 | 4/15 |
| backend-parity-c/vectored-io | 1,680,463 | 3,065 | 3/13 |
| c-programs/ioctl-fioclex | 1,924,698 | 3,265 | 3/14 |
| c-programs/pause-alarm-interrupt | 2,014,495 | 3,393 | 4/14 |
| system-utils/clock-determinism | 1,809,704 | 3,229 | 3/13 |
| system-utils/record-getpid | 1,714,785 | 3,090 | 3/13 |

All derived bounds fit the unchanged **22 CPU / 57 wall seconds**. Even using
each cell's maximum sample instead of p90 produces at most 7 CPU / 30 wall
seconds. The new per-cell calibration is separate from the frozen 492-cell
census and every existing KVM/other-lane calibration. All six existing explicit
timeout pairs remain unchanged. Selection adds 22 required cells and subtracts
22 enabled non-CI cells; it does not rewrite those earlier measurements.

## Historical status and retained provenance

Before this change all 22 cells were enabled red and `measured-and-passed`:
red represented their unselected status, with older passing observations still
present. Nineteen demotion reasons cite Hermit
`8f6d8a57d6a8e4ef7fa0ae14c16e2a494a2c0ab5` and task
`ov-backend-ratchet-fanout`: a `/proc/self/maps` COMMIT divergence at turn 10,
20 of 26 candidates diverging at the shared startup point, and later failures
in two of six apparent three-pass survivors. The two reasons for
`personality-domain` and `vectored-io` instead cite
`17107307a33b6f6383c1b7bc2acb364b162b6c8c`; `cwd-roundtrip` cites
`e73e7ba0bf56fbbfefffa304f7622c67aac6ea88`. Those report inode-width-dependent
PMU drift accumulating to 672 RCBs (6,720 ns) at turn 10. None of this failure
evidence is reclassified or erased by these ten new repetitions. Only the promoted cells'
now-obsolete `ci_disabled_reason` entries are removed. Their original manifest
and cell records remain in base commit
`26bda94103ad20cf1d572bac5bc50217826eebed` and the retained proposal evidence.

The real summary was imported with the supported
`scorecard.rs update-observations --summary <red22/summary.json> --retained`
before editing selection. It imported all 220 rows across exactly 22 cells,
skipped zero, and preserved prior observations and the newer-evidence rule.
`scorecard.rs update` subsequently derives selection without manufacturing
observations. The committed pressure selector is unchanged by this promotion.

The parent workspace retains the campaign under
`ignored/liteinst-01a0a13c-review/queue-drain-20260916/hermit-2948-campaign/`;
`quota-path.txt` identifies its owned quota directory and `red22/summary.json`
is the imported file. Proposal evidence is in the sibling
`hermit-2948-promotion-22/`, including all 220 raw-result hashes, calibration,
base records, complete diff and command results. The independent audit is in
`hermit-2948-red-independent/`:

- Summary SHA-256: `1ef30e72988c66cad379c4d8631354f15e52cfdf89b3067f574afeaae7664f0a`.
- Audit report: 19,609 bytes, SHA-256 `b3ea07e527b1db3318fa88119e25f7d28fe8fe72df346c15f190124567080c94`.
- Full audit JSON SHA-256: `80bf336c42da58c77919ddc8e472eaffe358c342ba0d0aea3c94341b2f5b3e4d`.

The promotion base `26bda94103ad20cf1d572bac5bc50217826eebed` differs from the
measured `ed99e05133b00058fafed7f3dfaf48ab8fd334e6` only in the previously
committed pressure selector. The expected plan, validation DAG and scorecard
were regenerated by their project tools. The generated DAG adds only result
ownership for these identities; its commands, dependencies and limits remain
unchanged.

Validation passed 310 manifest package tests in 11.853 seconds and the full
metadata front door in 407.507 seconds (seven independent audits, 13 manifests,
756 required cells). The latter includes scorecard history/admission checks,
the pressure runner self-test and validation-driver self-test; their fixture
runs are not additional backend measurements. Clippy with warnings denied and
workspace formatting passed. The initial test run's four stale-DAG failures
remain retained; regenerating that artifact satisfied the original checks.

This evidence supports selection of the named cells on the measured source.
It does not establish universal determinism, memory determinism, replay/chaos
coverage, lower overhead, arbitrary TLS/fork safety or full LiteInst readiness.
