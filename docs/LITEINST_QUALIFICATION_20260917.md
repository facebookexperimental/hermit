# LiteInst qualification: 96 verify cells, 2026-09-17

This change enables and selects 96 previously disabled LiteInst `verify` cells
using the first four independently accepted batches of the existing official
212-cell disabled campaign. Each cell has ten qualifying first attempts: 960
strict canonical verification pairs, with 1,920 complete retained INFO logs.
The measured source is Hermit `26bda94103ad20cf1d572bac5bc50217826eebed`,
Detcore tree `779213274890cd37daafae32ea5a73c1f84090a6`. The imported
observations preserve those historical identities, invocation metadata and
comparison history. No new guest execution was performed by this change.

The author base is `cc08c9b71a21643606636ab57a99e569ede416d7`, whose Detcore
tree is `9f6359bab9d457808be23af6aa173ff74b1f8be6`. The shared physical-exit
path changed between the measured source and this base: it acknowledges an
exiting robust-list owner's logical clock through an ordinary RPC before
releasing the group's staged wake batch. This is shared runtime code, including
the host hybrid, and is not a metadata-only difference. The same three-file
delta received an interaction review for the prior 22-cell promotion in
[the earlier promotion](https://github.com/rrnewton/hermit/pull/2948).
Under the owner's soft-green landing rule, these accepted historical results
support selection without an exact-current-source rerun as a prerequisite.
They do not prove source equivalence or fresh parity on the changed runtime.
Current-source verification remains follow-up.

The historical executions use the activated **LiteInst host hybrid with the
ptrace Detcore Tool**. They are same-backend strict repeat comparisons, not
in-process LiteInst, ptrace-free execution, or comparisons against a ptrace
golden run. The category name `backend-parity-c` is not evidence of
cross-backend parity. The unchanged command policy uses `--log info`,
`--strict --verify --verify-strict`, `BitwiseInfoV1`, positive compared INFO
counts, exact exit/stdout/stderr comparison and no relaxations. The independent
audits checked complete canonical log pairs, no truncation, one outer attempt,
one inner verification, and no retries, omitted failed attempts or no-result
rows in this population. Staging and activation succeeded before the first
measured cell in each batch. This qualification is not a measurement of a
missing runtime.

Comparable selection changes from 753 to 849 out of 5,760 cells, and enabled
cells increase from 907 to 1,003. The 154 enabled-but-unselected cells remain
unchanged; not-applicable cells decrease from 4,853 to 4,757. LiteInst verify
changes from 50 selected / 3 enabled but unselected / 307 disabled to
146 / 3 / 211. Only the 96 cells below are newly qualified by this evidence;
the earlier 50 selected cells are not freshly measured here. These are
selection counts, not a backend determinism percentage. The remaining 116
cells from the separate 212-cell campaign are outside this change. All other
backends and modes retain their original selection and observation history.

The full manifest plan also includes three custom entries outside the
comparable matrix, so its required count changes from 756 to 852. The 95 added
portable identities each have one ordinary and one hosted validation result
owner. The privileged `cpuid-probe` identity has the existing full, privileged
and hosted-privileged owners, making 193 added result entries in total. Existing
timeouts, calibration census, prior 22-cell records and all unrelated validation
commands, resources and ownership remain unchanged.

## Evidence and runtime identity

All four batches used `--probe-disabled --backend liteinst --repetitions 10
--jobs 8 --manifest-guest-cap 8 --run-timeout 21600` with an exact per-batch
cells file. The ordinary CPU/wall bounds stay **22/57 seconds**, with scale
multipliers of 1.0; portable memory remains 3 GiB and privileged memory 16 GiB.
The 1 GiB logical per-log cap and 64 GiB compressed aggregate quota remain
enabled. These changes do not widen a timeout, resource allowance, comparison
policy, retry limit or admission rule.

The measurements used Reverie `a158914eceeca02a9ab4c7dd4e9916926d5e5c1e`
and LiteInst2 `95ee5e6917fa33191eb41c3f1606ea8b03c1b78c`. The measured LiteInst
runtime SHA256 is
`3e388b388ca7167a10cbba8c3a79c5e1b596691788c4ce1c642d81a7426dfa7e`.
The complete Hermit ELF, LiteInst runtime, revision marker and published
artifact manifests were retained for each batch; the independent audits
rehashed those files. Other backend resource bytes are bound by published
manifests but are not claimed as retained or independently qualified here.

The original campaign is retained in the parent workspace beneath
`ignored/liteinst-01a0a13c-review/queue-drain-20260916/hermit-2948-campaign/
brs-liteinst-2948-3146559-1789596277480275238` (one path; line-wrapped here).
Independent reports and freezes are in the neighboring
`hermit-2948-disabled-01-independent` through `-04-independent` directories.
The author evidence is in `hermit-disabled-promotion-96-implementation`.

| Batch | Summary SHA256 | Independent report SHA256 |
| --- | --- | --- |
| disabled-01 | `625aef2c68be685b5dee024659f3fa536997eab7ac9139a116a8fe68e0552e59` | `bd778029de031792f96f5f60e02ca0f2be4fc85d7bd04893f4d29a7437f935e3` |
| disabled-02 | `baf20f2ccf07aa2dc0e9600b49778b02e71dcb951dcb924bfb8ec4e5272285ae` | `e8050e3337134b98a48333239b1a3ba276bcf804c2e6fa40a0f0667e87907b01` |
| disabled-03 | `aa43e424835260a00dea5e564bbcddfdab019ce265af81f59f10e1144ddfe340` | `d579659c38ccfaa3f10a876ead47c257062c19c73cf4b1eba4edeb5fc1ba70e6` |
| disabled-04 | `8921d509ff4aff23ab3172651b5c7a7e1b30935e8b6b024beee7b54a88dfdf33` | `212363de49acf6bdbdf47ef564de8e5f14506ed18327ecde0b342e2b4e1f898d` |

| Batch | Executed Hermit ELF SHA256 | Complete artifact identity |
| --- | --- | --- |
| disabled-01 | `c2a63d5ce9107f6b3e2cf7e1d99ce89db2ea4b833481aa818204a60ae184b888` | `e3af88db5afc8fd11093dbbab639bbc63646fa1403888c4d840e2419fb2e391d` |
| disabled-02 | `c592abf402c57f56ae608b2775bed70d09b2d4bc2335d3b0379d09fe98ea752a` | `9f5a389ec9ddaff8350d3a207bc4b6dfafa43628456c0bfa119624bb387509ac` |
| disabled-03 | `b5d4a19b37bdc2864aa848b410ba4a7b03abc7f9b38d13dfae72e69d8e765695` | `2b939a4cc47c512c061510bc8a9ef3cf4cc9c2ddfd08b50c39b3fc05080bc253` |
| disabled-04 | `91239ddee322e25e915ae07c2b19c1c5a276956f345b2b2a5f8d8882de61fae1` | `b3cfc51229cddbe7ac9d2885729bf84295583a11d7a6be0d2cb5e49a8da0f19d` |

## Per-cell timeout calibration

Every row has `mode=verify`, `backend=liteinst` and ten samples. The category is
the test prefix; every row is portable except `backend-parity-c/cpuid-probe`,
which remains privileged. CPU uses the raw row's aggregate `cpu_usage_usec`;
wall uses its complete `duration_ms`, including reported preparation. The
nearest-rank p90 is the ninth of ten independently sorted values. The existing
formula is `ceil(1.5 × p90 CPU)` and `ceil(4 × p90 wall)`, using threefold wall
only where the fourfold value exceeds 120 seconds. No outlier is discarded.

The largest derived p90 bounds are 15 CPU / 50 wall seconds. Even using the
maximum samples yields at most 15 / 52, from
`backend-parity-c/readdir-order-identity`, within the unchanged 22/57 limits.
That slow case and the privileged CPUID case remain in both the records and
ordinary selection. The separate dated Rust calibration array extends the
existing formula and required-selection checks without rewriting the frozen
timeout census. Newly enabled cells add equally to enabled and required, so
they do not reduce the historical enabled-but-unselected count again.

The author calibration.json is SHA256
`b37ed5af1f236b742657a074492d18ef6f49d113b986b16e40d9e9d57455e755`.
It records every one of the 960 raw result paths, byte counts and SHA256 values,
all ten CPU/wall samples for each cell, independently sorted p90 and maximum
values, and the configured/derived bounds. Those hashes identify the original
rows; neither a summary nor a timing sample was synthesized.

| Test | Batch | p90 CPU (µs) | p90 wall (ms) | Derived CPU/wall (s) |
| --- | --- | ---: | ---: | ---: |
| backend-parity-c/append-pwrite | disabled-01 | 1,832,898 | 3,580 | 3/15 |
| backend-parity-c/bind-getsockname | disabled-01 | 1,688,604 | 3,108 | 3/13 |
| backend-parity-c/cachestat-refusal | disabled-01 | 1,769,174 | 3,105 | 3/13 |
| backend-parity-c/child-subreaper-refusal | disabled-01 | 1,680,221 | 3,026 | 3/13 |
| backend-parity-c/close-range-fds | disabled-01 | 1,689,260 | 3,062 | 3/13 |
| backend-parity-c/copy-file-range-refusal | disabled-01 | 1,744,373 | 3,130 | 3/13 |
| backend-parity-c/cpu-virtualization | disabled-01 | 1,811,671 | 3,129 | 3/13 |
| backend-parity-c/dup-shared-offset | disabled-01 | 1,728,572 | 3,149 | 3/13 |
| backend-parity-c/epoll-pwait2 | disabled-01 | 1,677,649 | 3,049 | 3/13 |
| backend-parity-c/epoll-readiness | disabled-01 | 1,650,912 | 3,008 | 3/13 |
| backend-parity-c/faccessat2-flags | disabled-01 | 1,693,160 | 3,014 | 3/13 |
| backend-parity-c/fadvise-hints | disabled-01 | 1,779,288 | 3,213 | 3/13 |
| backend-parity-c/fallocate-extents | disabled-01 | 1,794,049 | 3,238 | 3/13 |
| backend-parity-c/fchmod-bits | disabled-01 | 1,818,502 | 3,567 | 3/15 |
| backend-parity-c/fchmodat2-flags | disabled-01 | 1,614,199 | 2,943 | 3/12 |
| backend-parity-c/fd-duplication | disabled-01 | 1,677,753 | 3,131 | 3/13 |
| backend-parity-c/file-backed-mmap | disabled-01 | 1,738,160 | 3,118 | 3/13 |
| backend-parity-c/flock-lifecycle | disabled-01 | 1,773,037 | 3,127 | 3/13 |
| backend-parity-c/fsync-durability | disabled-01 | 1,881,428 | 3,191 | 3/13 |
| backend-parity-c/ftruncate-sparse | disabled-01 | 1,682,296 | 3,105 | 3/13 |
| backend-parity-c/getcpu-identity | disabled-01 | 1,851,621 | 3,219 | 3/13 |
| backend-parity-c/getpriority-identity | disabled-01 | 1,603,569 | 2,991 | 3/12 |
| backend-parity-c/hardware-trap-identity | disabled-01 | 1,906,337 | 5,682 | 3/23 |
| backend-parity-c/host-identity | disabled-02 | 1,594,430 | 2,846 | 3/12 |
| backend-parity-c/inline-syscall-sites | disabled-02 | 1,624,046 | 2,909 | 3/12 |
| backend-parity-c/inotify-watch | disabled-02 | 1,859,000 | 3,110 | 3/13 |
| backend-parity-c/ioctl-fionread | disabled-02 | 1,851,234 | 3,220 | 3/13 |
| backend-parity-c/kcmp-refusal | disabled-02 | 1,681,324 | 3,074 | 3/13 |
| backend-parity-c/linkat-flags | disabled-02 | 1,770,012 | 3,107 | 3/13 |
| backend-parity-c/lseek-positioning | disabled-02 | 1,697,584 | 3,016 | 3/13 |
| backend-parity-c/mce-kill-refusal | disabled-02 | 1,773,989 | 3,103 | 3/13 |
| backend-parity-c/memfd-create | disabled-02 | 1,774,282 | 3,210 | 3/13 |
| backend-parity-c/mempolicy-default | disabled-02 | 1,700,178 | 3,183 | 3/13 |
| backend-parity-c/mincore-residency | disabled-02 | 1,698,075 | 3,033 | 3/13 |
| backend-parity-c/mixed-inline-and-libc-syscalls | disabled-02 | 1,763,313 | 3,096 | 3/13 |
| backend-parity-c/mknod-special | disabled-02 | 1,951,363 | 3,257 | 3/14 |
| backend-parity-c/msync-writeback | disabled-02 | 1,729,079 | 3,147 | 3/13 |
| backend-parity-c/name-to-handle-refusal | disabled-02 | 1,929,776 | 3,258 | 3/14 |
| backend-parity-c/no-new-privs-refusal | disabled-02 | 1,695,564 | 3,055 | 3/13 |
| backend-parity-c/numa-node-identity | disabled-02 | 1,871,997 | 3,227 | 3/13 |
| backend-parity-c/openat-flags | disabled-02 | 1,665,762 | 3,164 | 3/13 |
| backend-parity-c/openat2-refusal | disabled-02 | 1,786,933 | 3,109 | 3/13 |
| backend-parity-c/path-file-ops | disabled-02 | 1,711,247 | 3,111 | 3/13 |
| backend-parity-c/pidfd-open-self | disabled-02 | 1,752,879 | 3,185 | 3/13 |
| backend-parity-c/pipe-capacity-pin | disabled-02 | 1,866,925 | 3,277 | 3/14 |
| backend-parity-c/pipe-ipc | disabled-02 | 1,668,560 | 3,023 | 3/13 |
| backend-parity-c/pipe2-flags | disabled-02 | 1,746,333 | 3,111 | 3/13 |
| backend-parity-c/poll-readiness | disabled-03 | 1,709,153 | 3,047 | 3/13 |
| backend-parity-c/prctl-identity | disabled-03 | 1,709,577 | 3,032 | 3/13 |
| backend-parity-c/prctl-pdeathsig | disabled-03 | 1,883,598 | 3,148 | 3/13 |
| backend-parity-c/preadv2-flags | disabled-03 | 1,824,312 | 3,161 | 3/13 |
| backend-parity-c/pthread-lifecycle | disabled-03 | 1,780,677 | 3,541 | 3/15 |
| backend-parity-c/readdir-entries | disabled-03 | 1,740,023 | 3,503 | 3/15 |
| backend-parity-c/readdir-order-identity | disabled-03 | 9,500,190 | 12,415 | 15/50 |
| backend-parity-c/rename-ops | disabled-03 | 1,707,139 | 3,114 | 3/13 |
| backend-parity-c/renameat2-flags | disabled-03 | 1,666,763 | 3,012 | 3/13 |
| backend-parity-c/rlimit-identity | disabled-03 | 1,749,417 | 3,042 | 3/13 |
| backend-parity-c/robust-list | disabled-03 | 1,796,965 | 3,101 | 3/13 |
| backend-parity-c/sched-getaffinity-identity | disabled-03 | 1,781,854 | 3,595 | 3/15 |
| backend-parity-c/seccomp-refusal | disabled-03 | 1,622,968 | 2,926 | 3/12 |
| backend-parity-c/short-io-split-identity | disabled-03 | 1,703,127 | 3,021 | 3/13 |
| backend-parity-c/shutdown-socketpair | disabled-03 | 1,602,921 | 2,990 | 3/12 |
| backend-parity-c/signal-waitstatus-identity | disabled-03 | 1,729,651 | 5,832 | 3/24 |
| backend-parity-c/signalfd-create | disabled-03 | 1,707,936 | 3,155 | 3/13 |
| backend-parity-c/socket-epoll-ordering | disabled-03 | 1,870,409 | 3,764 | 3/16 |
| backend-parity-c/socket-options | disabled-03 | 1,648,150 | 3,057 | 3/13 |
| backend-parity-c/socketpair-flags | disabled-03 | 1,690,636 | 2,995 | 3/12 |
| backend-parity-c/sockname-unnamed | disabled-03 | 1,771,363 | 3,203 | 3/13 |
| backend-parity-c/statfs-free-determinism | disabled-03 | 1,630,318 | 3,027 | 3/13 |
| backend-parity-c/statx-metadata | disabled-03 | 1,602,812 | 2,950 | 3/12 |
| backend-parity-c/sync-file-range | disabled-03 | 1,670,801 | 3,046 | 3/13 |
| backend-parity-c/sysv-ipc-refusal | disabled-04 | 1,758,770 | 3,042 | 3/13 |
| backend-parity-c/thp-disable | disabled-04 | 1,591,929 | 2,912 | 3/12 |
| backend-parity-c/uname-identity | disabled-04 | 1,665,613 | 3,070 | 3/13 |
| backend-parity-c/utimensat-determinism | disabled-04 | 1,753,246 | 3,118 | 3/13 |
| backend-parity-c/vectored-file-io | disabled-04 | 1,748,805 | 3,043 | 3/13 |
| c-programs/acct-refusal-probe | disabled-04 | 1,863,522 | 3,158 | 3/13 |
| c-programs/add-key-enosys | disabled-04 | 1,736,037 | 3,033 | 3/13 |
| c-programs/adjtimex-deterministic | disabled-04 | 1,666,804 | 3,106 | 3/13 |
| c-programs/bpf-enosys | disabled-04 | 1,802,251 | 3,360 | 3/14 |
| c-programs/cachestat-enosys | disabled-04 | 1,762,500 | 3,060 | 3/13 |
| c-programs/clock-adjtime-deterministic | disabled-04 | 1,711,479 | 3,083 | 3/13 |
| c-programs/clone | disabled-04 | 1,673,058 | 3,021 | 3/13 |
| c-programs/copy-file-range-refusal-probe | disabled-04 | 1,718,281 | 3,037 | 3/13 |
| c-programs/dbt-copied-tiocgpgrp | disabled-04 | 1,752,948 | 3,427 | 3/14 |
| c-programs/dbt-exec-failure | disabled-04 | 1,677,976 | 2,999 | 3/12 |
| c-programs/dbt-mmap-exec | disabled-04 | 1,678,470 | 3,172 | 3/13 |
| c-programs/dbt-prlimit-self | disabled-04 | 1,684,683 | 2,999 | 3/12 |
| c-programs/dbt-self-sigqueue | disabled-04 | 1,799,892 | 3,173 | 3/13 |
| c-programs/dbt-wait-lifecycle | disabled-04 | 2,060,224 | 3,936 | 4/16 |
| c-programs/fp-reduction-nondeterminism | disabled-04 | 2,423,709 | 3,946 | 4/16 |
| c-programs/futex-requeue-enosys | disabled-04 | 1,754,734 | 3,233 | 3/13 |
| c-programs/futex-waitv-enosys | disabled-04 | 1,648,814 | 3,011 | 3/13 |
| c-programs/futex-wake-enosys | disabled-04 | 1,661,738 | 2,960 | 3/12 |
| c-programs/get-robust-list-child | disabled-04 | 1,593,175 | 2,902 | 3/12 |
| backend-parity-c/cpuid-probe | disabled-01 | 1,772,262 | 5,897 | 3/24 |

## Historical disabled reasons

Only these obsolete LiteInst disabled entries are removed from the two
manifests. Other backends' reasons remain unchanged. Sixty-five recipes already
had `ci: true`; 31 existing per-backend CI mappings gain only `liteinst: true`.
The original reasons remain below and in the immutable base so selection is
not confused with a claim that an old failure never happened. The scorecard had
no stored measurement for these cells; that state alone is not evidence of an
execution failure. Their previous measurement metadata was `never-measured`.

| Test | Original LiteInst disabled reason |
| --- | --- |
| backend-parity-c/append-pwrite | Not evaluated in the source backend-parity matrix; qualify LiteInst separately |
| backend-parity-c/bind-getsockname | Not evaluated in the source backend-parity matrix; qualify LiteInst separately |
| backend-parity-c/cachestat-refusal | Not evaluated in the source backend-parity matrix; qualify LiteInst separately |
| backend-parity-c/child-subreaper-refusal | Not evaluated in the source backend-parity matrix; qualify LiteInst separately |
| backend-parity-c/close-range-fds | Not evaluated in the source backend-parity matrix; qualify LiteInst separately |
| backend-parity-c/copy-file-range-refusal | Not evaluated in the source backend-parity matrix; qualify LiteInst separately |
| backend-parity-c/cpu-virtualization | Not evaluated in the source backend-parity matrix; qualify LiteInst separately |
| backend-parity-c/cpuid-probe | Initial C-corpus migration preserves the established ptrace baseline; qualify LiteInst separately |
| backend-parity-c/dup-shared-offset | Not evaluated in the source backend-parity matrix; qualify LiteInst separately |
| backend-parity-c/epoll-pwait2 | Not evaluated in the source backend-parity matrix; qualify LiteInst separately |
| backend-parity-c/epoll-readiness | Not evaluated in the source backend-parity matrix; qualify LiteInst separately |
| backend-parity-c/faccessat2-flags | Not evaluated in the source backend-parity matrix; qualify LiteInst separately |
| backend-parity-c/fadvise-hints | Not evaluated in the source backend-parity matrix; qualify LiteInst separately |
| backend-parity-c/fallocate-extents | Not evaluated in the source backend-parity matrix; qualify LiteInst separately |
| backend-parity-c/fchmod-bits | Not evaluated in the source backend-parity matrix; qualify LiteInst separately |
| backend-parity-c/fchmodat2-flags | Not evaluated in the source backend-parity matrix; qualify LiteInst separately |
| backend-parity-c/fd-duplication | Not evaluated in the source backend-parity matrix; qualify LiteInst separately |
| backend-parity-c/file-backed-mmap | Not evaluated in the source backend-parity matrix; qualify LiteInst separately |
| backend-parity-c/flock-lifecycle | Not evaluated in the source backend-parity matrix; qualify LiteInst separately |
| backend-parity-c/fsync-durability | Not evaluated in the source backend-parity matrix; qualify LiteInst separately |
| backend-parity-c/ftruncate-sparse | Not evaluated in the source backend-parity matrix; qualify LiteInst separately |
| backend-parity-c/getcpu-identity | Not evaluated in the source backend-parity matrix; qualify LiteInst separately |
| backend-parity-c/getpriority-identity | Not evaluated in the source backend-parity matrix; qualify LiteInst separately |
| backend-parity-c/hardware-trap-identity | Unmeasured for this guest; qualify LiteInst separately |
| backend-parity-c/host-identity | Not evaluated in the source backend-parity matrix; qualify LiteInst separately |
| backend-parity-c/inline-syscall-sites | LiteInst preload runtime is not built beside the binary on this host |
| backend-parity-c/inotify-watch | Not evaluated in the source backend-parity matrix; qualify LiteInst separately |
| backend-parity-c/ioctl-fionread | Not evaluated in the source backend-parity matrix; qualify LiteInst separately |
| backend-parity-c/kcmp-refusal | Not evaluated in the source backend-parity matrix; qualify LiteInst separately |
| backend-parity-c/linkat-flags | Not evaluated in the source backend-parity matrix; qualify LiteInst separately |
| backend-parity-c/lseek-positioning | Not evaluated in the source backend-parity matrix; qualify LiteInst separately |
| backend-parity-c/mce-kill-refusal | Not evaluated in the source backend-parity matrix; qualify LiteInst separately |
| backend-parity-c/memfd-create | Not evaluated in the source backend-parity matrix; qualify LiteInst separately |
| backend-parity-c/mempolicy-default | Not evaluated in the source backend-parity matrix; qualify LiteInst separately |
| backend-parity-c/mincore-residency | Not evaluated in the source backend-parity matrix; qualify LiteInst separately |
| backend-parity-c/mixed-inline-and-libc-syscalls | LiteInst preload runtime is not built beside the binary on this host |
| backend-parity-c/mknod-special | Not evaluated in the source backend-parity matrix; qualify LiteInst separately |
| backend-parity-c/msync-writeback | Not evaluated in the source backend-parity matrix; qualify LiteInst separately |
| backend-parity-c/name-to-handle-refusal | Not evaluated in the source backend-parity matrix; qualify LiteInst separately |
| backend-parity-c/no-new-privs-refusal | Not evaluated in the source backend-parity matrix; qualify LiteInst separately |
| backend-parity-c/numa-node-identity | Not evaluated in the source backend-parity matrix; qualify LiteInst separately |
| backend-parity-c/openat-flags | Not evaluated in the source backend-parity matrix; qualify LiteInst separately |
| backend-parity-c/openat2-refusal | Not evaluated in the source backend-parity matrix; qualify LiteInst separately |
| backend-parity-c/path-file-ops | Not evaluated in the source backend-parity matrix; qualify LiteInst separately |
| backend-parity-c/pidfd-open-self | Not evaluated in the source backend-parity matrix; qualify LiteInst separately |
| backend-parity-c/pipe-capacity-pin | LiteInst canonical verification remains blocked by guest-visible startup mapping identity |
| backend-parity-c/pipe-ipc | Not evaluated in the source backend-parity matrix; qualify LiteInst separately |
| backend-parity-c/pipe2-flags | Not evaluated in the source backend-parity matrix; qualify LiteInst separately |
| backend-parity-c/poll-readiness | Not evaluated in the source backend-parity matrix; qualify LiteInst separately |
| backend-parity-c/prctl-identity | Not evaluated in the source backend-parity matrix; qualify LiteInst separately |
| backend-parity-c/prctl-pdeathsig | Not evaluated in the source backend-parity matrix; qualify LiteInst separately |
| backend-parity-c/preadv2-flags | Not evaluated in the source backend-parity matrix; qualify LiteInst separately |
| backend-parity-c/pthread-lifecycle | Initial C-corpus migration preserves the established ptrace baseline; qualify LiteInst separately |
| backend-parity-c/readdir-entries | Not evaluated in the source backend-parity matrix; qualify LiteInst separately |
| backend-parity-c/readdir-order-identity | LiteInst preload runtime libreverie_liteinst.so is not built beside the binary; qualify LiteInst separately |
| backend-parity-c/rename-ops | Not evaluated in the source backend-parity matrix; qualify LiteInst separately |
| backend-parity-c/renameat2-flags | Not evaluated in the source backend-parity matrix; qualify LiteInst separately |
| backend-parity-c/rlimit-identity | Not evaluated in the source backend-parity matrix; qualify LiteInst separately |
| backend-parity-c/robust-list | Not evaluated in the source backend-parity matrix; qualify LiteInst separately |
| backend-parity-c/sched-getaffinity-identity | Not evaluated in the source backend-parity matrix; qualify LiteInst separately |
| backend-parity-c/seccomp-refusal | Not evaluated in the source backend-parity matrix; qualify LiteInst separately |
| backend-parity-c/short-io-split-identity | Not yet qualified for the partial-transfer split contract |
| backend-parity-c/shutdown-socketpair | Not evaluated in the source backend-parity matrix; qualify LiteInst separately |
| backend-parity-c/signal-waitstatus-identity | Unmeasured for this guest; qualify LiteInst separately |
| backend-parity-c/signalfd-create | Not evaluated in the source backend-parity matrix; qualify LiteInst separately |
| backend-parity-c/socket-epoll-ordering | Never measured for this cell; the original .toml cited a missing toolchain on the authoring host, which is not evidence about the backend |
| backend-parity-c/socket-options | Not evaluated in the source backend-parity matrix; qualify LiteInst separately |
| backend-parity-c/socketpair-flags | Not evaluated in the source backend-parity matrix; qualify LiteInst separately |
| backend-parity-c/sockname-unnamed | Not evaluated in the source backend-parity matrix; qualify LiteInst separately |
| backend-parity-c/statfs-free-determinism | Not evaluated in the source backend-parity matrix; qualify LiteInst separately |
| backend-parity-c/statx-metadata | Not evaluated in the source backend-parity matrix; qualify LiteInst separately |
| backend-parity-c/sync-file-range | Not evaluated in the source backend-parity matrix; qualify LiteInst separately |
| backend-parity-c/sysv-ipc-refusal | Not evaluated in the source backend-parity matrix; qualify LiteInst separately |
| backend-parity-c/thp-disable | Not evaluated in the source backend-parity matrix; qualify LiteInst separately |
| backend-parity-c/uname-identity | Not evaluated in the source backend-parity matrix; qualify LiteInst separately |
| backend-parity-c/utimensat-determinism | Not evaluated in the source backend-parity matrix; qualify LiteInst separately |
| backend-parity-c/vectored-file-io | Not evaluated in the source backend-parity matrix; qualify LiteInst separately |
| c-programs/acct-refusal-probe | Initial C-corpus migration preserves the established ptrace baseline; qualify LiteInst separately |
| c-programs/add-key-enosys | Initial C-corpus migration preserves the established ptrace baseline; qualify LiteInst separately |
| c-programs/adjtimex-deterministic | Initial C-corpus migration preserves the established ptrace baseline; qualify LiteInst separately |
| c-programs/bpf-enosys | Initial C-corpus migration preserves the established ptrace baseline; qualify LiteInst separately |
| c-programs/cachestat-enosys | Initial C-corpus migration preserves the established ptrace baseline; qualify LiteInst separately |
| c-programs/clock-adjtime-deterministic | Initial C-corpus migration preserves the established ptrace baseline; qualify LiteInst separately |
| c-programs/clone | Initial C-corpus migration preserves the established ptrace baseline; qualify LiteInst separately |
| c-programs/copy-file-range-refusal-probe | Initial C-corpus migration preserves the established ptrace baseline; qualify LiteInst separately |
| c-programs/dbt-copied-tiocgpgrp | Initial C-corpus migration preserves the established ptrace baseline; qualify LiteInst separately |
| c-programs/dbt-exec-failure | Initial C-corpus migration preserves the established ptrace baseline; qualify LiteInst separately |
| c-programs/dbt-mmap-exec | Initial C-corpus migration preserves the established ptrace baseline; qualify LiteInst separately |
| c-programs/dbt-prlimit-self | Initial C-corpus migration preserves the established ptrace baseline; qualify LiteInst separately |
| c-programs/dbt-self-sigqueue | Initial C-corpus migration preserves the established ptrace baseline; qualify LiteInst separately |
| c-programs/dbt-wait-lifecycle | Initial C-corpus migration preserves the established ptrace baseline; qualify LiteInst separately |
| c-programs/fp-reduction-nondeterminism | Initial C-corpus migration preserves the established ptrace baseline; qualify LiteInst separately |
| c-programs/futex-requeue-enosys | Initial C-corpus migration preserves the established ptrace baseline; qualify LiteInst separately |
| c-programs/futex-waitv-enosys | Initial C-corpus migration preserves the established ptrace baseline; qualify LiteInst separately |
| c-programs/futex-wake-enosys | Initial C-corpus migration preserves the established ptrace baseline; qualify LiteInst separately |
| c-programs/get-robust-list-child | Initial C-corpus migration preserves the established ptrace baseline; qualify LiteInst separately |

These results do not establish arbitrary-program determinism, Linux semantic
equivalence for unsupported paths, lower overhead, replay/chaos or memory
determinism, in-process fork/TLS safety, or readiness to replace ptrace for all
serious work. The exact historical 96-cell host-hybrid evidence is the scope
of this selection. The changed shared robust-exit runtime and the remaining
backend gaps retain their separate verification obligations.
