# Overnight Report Final: 2026-07-24

> Snapshot: 2026-07-24T09:18:27Z. Landing window: 2026-07-24T00:00:00Z through 2026-07-24T09:18:27Z. Hermit `main`: `b0510255021f82333e99b04f0eb829273d4aa393`.

## Executive briefing

- **39 pull requests landed across the two development repositories:** 32 in [`rrnewton/hermit`](https://github.com/rrnewton/hermit/pulls?q=is%3Apr+is%3Amerged) and 7 in [`rrnewton/reverie`](https://github.com/rrnewton/reverie/pulls?q=is%3Apr+is%3Amerged). The Hermit merge commits were independently checked as ancestors of the report SHA.
- **Ptrace compatibility is 61/61 at L2** on the report SHA: `hermit run --strict --verify`, default log level, no relaxations.
- **The alternate-mode baselines remain partial:** DBI 20/38 at L2, KVM 31/57 at L2, and output-correct record/replay 36/57. These use different historical corpus sizes and exact SHAs; they are not a same-denominator leaderboard.
- **QEMU reached L2 under the ptrace backend.** The strict Linux boot marker was observed twice and verifier logs matched: 1,085,768 messages per run and 817,137 DETLOG/scheduler COMMIT entries per run.
- **Eight syscall-control areas landed:** `ppoll`, `waitid`, `prlimit64`, `arch_prctl`, `getrandom`, scheduler affinity, `writev`, and `madvise`.
- **The ratcheted strict compatibility gate grew 16 -> 32 -> 38 -> 57 -> 61** through PRs #521, #537, #542, #550, and #554.
- **Primary remaining gates:** human review for Python/vfork PR #239; native DBI process-tree/fork work tracked by Reverie issue #31; KVM fork/clone tracked by Reverie issue #55; and record/replay descriptor-state fixes in PR #240 plus issue #536.

## Measurement contract

- L2 means `hermit run --strict --verify`: two strict executions with no substantive verifier-log difference.
- Every Hermit run below names backend, log level, and relaxations. `none` means no determinism-weakening flag was used.
- Record/replay is reported separately from L1-L4. The tested recording CLI does not expose the same strict-mode contract; an R/R pass requires record exit 0, replay exit 0, and byte-identical guest stdout.
- The compatibility corpus changed during the window. DBI was measured against 38 rows; KVM and R/R against 57; current ptrace against 61. The report preserves those denominators rather than extrapolating unmeasured rows.
- Filesystem, host service, and external network state are not snapshotted. The Python result below distinguishes the deterministic vfork fix from live NSCD socket readiness.

## Test context

| Field | Value |
| --- | --- |
| Report repository/branch | `rrnewton/hermit`, `overnight-final-report-slot62-v2` |
| Hermit report SHA | `b0510255021f82333e99b04f0eb829273d4aa393` |
| Host | x86_64 Linux, AMD EPYC 9D85 158-Core Processor |
| Kernel | `6.17.13-0_fbk0_crackerjackhost_0_g2b4321c50d79` |
| `perf_event_paranoid` | `1` |
| `/dev/kvm` | present, `crw-rw-rw-`, owner `root:kvm` |
| Rust | `rustc 1.99.0-nightly (be8e82435 2026-07-11)` |
| Cargo | `cargo 1.99.0-nightly (59800466c 2026-07-07)` |
| cargo-nextest | `0.9.140` |
| Runtimes present | Python, Node.js, GCC/G++, Make, Redis, SQLite, and Java |

## Compatibility matrices

| Mode | Exact tested SHA | Pass | Fail | Rate | Assurance and run context |
| --- | --- | ---: | ---: | ---: | --- |
| ptrace | `b051025` | 61/61 | 0 | 100.0% | L2, ptrace, default log, relaxations none |
| DBI | `3c49d19` | 20/38 | 18 | 52.6% | L2, DBI, default log, relaxations none |
| KVM | `2df293b` | 31/57 | 26 | 54.4% | L2, KVM, default log, relaxations none |
| Record/replay | `2df293b` | 36/57 | 21 | 63.2% | output-correct R/R, ptrace transport, default log, relaxations none; not L2 |

### Ptrace: 61/61 L2

Live report command:

```text
with-proxy env VALIDATE_VERBOSE=1 ./validate.sh --strict-compat-only --no-label-pr
```

Observed output: `Strict compatibility envelope (61/61 passed L2)` and exit 0. Backend: ptrace. Log level: default. Relaxations: none. Full log: `/tmp/hermit-validate.7R7PLl.log`.

All current rows passed: `echo`, `seq`, `cat`, `wc`, `head`, `base64`, `id`, `lua`, `perl`, `awk`, `bc`, `sqlite3`, `bash`, `cargo`, `rustc`, `node`, `gcc`, `g++`, `make`, `bzip2`, `gzip`, `xz`, `zstd`, `openssl`, `sort`, `uniq`, `tr`, `cut`, `tee`, `paste`, `comm`, `join`, `find`, `stat`, `file`, `basename`, `dirname`, `env`, `printenv`, `uname`, `factor`, `expr`, `dd`, `df`, `du`, `hostname`, `whoami`, `groups`, `tty`, `nproc`, `arch`, `realpath`, `readlink`, `mktemp`, `sha256sum`, `sha1sum`, `md5sum`, `wc-lines`, `nl`, `expand`, and `unexpand`.

### DBI: 20/38 L2

Exact tested SHA: `3c49d197b4734a068860cb30954bc657b90abf09`. Backend: DBI/DynamoRIO. Log level: default. Relaxations: none. Same-SHA ptrace control: 38/38 L2.

Passed (20): `echo`, `seq`, `cat`, `wc`, `head`, `base64`, `id`, `lua`, `perl`, `awk`, `sqlite3`, `bash`, `openssl`, `find`, `stat`, `basename`, `dirname`, `uname`, `factor`, `expr`.

Failed (18):

- Seventeen 60-second DBI Run1 timeouts: `bc`, `cargo`, `rustc`, `bzip2`, `gzip`, `xz`, `zstd`, `sort`, `uniq`, `tr`, `cut`, `tee`, `paste`, `comm`, `join`, `env`, `printenv`.
- One loader rejection: `file` exited 255 because DynamoRIO could not read ELF headers from the resolved `/usr/local/bin/file` script.

The failures cluster around process creation/exec and pipeline lifecycle, but the matrix alone does not prove one root cause for all 17 timeouts. Reverie [issue #31](https://github.com/rrnewton/reverie/issues/31) tracks native-client process-tree `ppid` work plus remaining timer/clock Guest stubs.

### KVM: 31/57 L2

Exact tested SHA: `2df293bde92bded0893fbe5eb83a633453dabcb0`. Backend: KVM. Log level: default. Relaxations: none. Same-SHA ptrace control: 57/57 L2. `/dev/kvm` was available and every row had a 60-second bound; no row timed out.

Passed (31): `echo`, `seq`, `cat`, `wc`, `head`, `base64`, `id`, `lua`, `perl`, `awk`, `sqlite3`, `bash`, `openssl`, `stat`, `basename`, `dirname`, `uname`, `factor`, `expr`, `du`, `hostname`, `whoami`, `groups`, `nproc`, `arch`, `realpath`, `readlink`, `sha256sum`, `sha1sum`, `md5sum`, `wc-lines`.

Failed (26):

- `clone`/fork returned `ENOSYS` (19): `bc`, `bzip2`, `gzip`, `xz`, `zstd`, `sort`, `uniq`, `tr`, `cut`, `tee`, `paste`, `comm`, `join`, `dd`, `tty`, `mktemp`, `nl`, `expand`, `unexpand`.
- Fixed ELF interpreter-base overlap (2): `cargo`, `rustc`.
- `execve` returned `ENOSYS` (2): `env`, `printenv`.
- `fcntl(F_SETFD)` plus `fchdir` gaps (1): `find`.
- mount metadata access plus `chdir` gap (1): `df`.
- top-level shebang/script loading unsupported (1): `file`.

Reverie [issue #55](https://github.com/rrnewton/reverie/issues/55) is the explicit fork/clone blocker and directly covers 19 of the 26 failing rows.

### Record/replay: 36/57 output-correct

Exact tested SHA: `2df293bde92bded0893fbe5eb83a633453dabcb0`. Transport/backend: ptrace. Log level: default for the matrix, INFO for focused diagnosis. Relaxations: none. This is not an L2 claim.

| Stage | Passed | Failed | Rate |
| --- | ---: | ---: | ---: |
| Strict L2 control | 57/57 | 0 | 100.0% |
| Recording completed | 57/57 | 0 | 100.0% |
| Replay exited 0 | 50/57 | 7 | 87.7% |
| Output-correct R/R | 36/57 | 21 | 63.2% |

Passed (36): `echo`, `seq`, `cat`, `wc`, `head`, `base64`, `id`, `lua`, `perl`, `awk`, `sqlite3`, `bash`, `openssl`, `find`, `stat`, `file`, `basename`, `dirname`, `env`, `printenv`, `uname`, `factor`, `expr`, `df`, `du`, `hostname`, `whoami`, `groups`, `nproc`, `arch`, `realpath`, `readlink`, `sha256sum`, `sha1sum`, `md5sum`, `wc-lines`.

Failed (21):

- Replay exit 0 but stdout mismatch (14): `bc`, `bzip2`, `gzip`, `zstd`, `sort`, `uniq`, `tr`, `cut`, `tee`, `dd`, `tty`, `nl`, `expand`, `unexpand`.
- Descriptor-number/order desync (5): `paste`, `comm`, `join`, `xz`, `mktemp`.
- Replay stream ended while expecting `EpollWait` (2): `cargo`, `rustc`; tracked by [issue #536](https://github.com/rrnewton/hermit/issues/536).

The focused trace points to numeric-fd-only stdout injection in `hermit-cli/src/replayer/fs.rs:130-138` and simple return-value substitution for descriptor-mutating calls in `hermit-cli/src/replayer/mod.rs:149-167`. Draft [PR #240](https://github.com/rrnewton/hermit/pull/240) performs the real kernel `close` during replay; it remains open, draft, and `human-review` gated.

## Milestones

### QEMU Linux boot reached L2

Merged [Hermit PR #553](https://github.com/rrnewton/hermit/pull/553) adds an opt-in `validate.sh --qemu-l2-only` and workflow-dispatch gate. The measured command was:

```text
./validate.sh --qemu-l2-only --no-label-pr --verbose
```

Observed: the L1 boot marker was present, then L2 compared 1,085,768/1,085,768 total messages and 817,137/817,137 DETLOG/scheduler COMMIT messages with no substantive difference. Run context: ptrace backend, INFO log, relaxations none.

This is QEMU running as a guest process under Hermit's ptrace backend. It is not the Hermit KVM backend and should not be reported as KVM compatibility.

### Eight syscall-control areas landed

| Area | Pull request | Result |
| --- | --- | --- |
| `ppoll` | [#273](https://github.com/rrnewton/hermit/pull/273) | deterministic nonblocking/scheduler handling |
| `waitid` | [#274](https://github.com/rrnewton/hermit/pull/274) | deterministic child-wait handling |
| `prlimit64` | [#534](https://github.com/rrnewton/hermit/pull/534) | deterministic self-resource handling |
| `arch_prctl` | [#539](https://github.com/rrnewton/hermit/pull/539) | deterministic architecture-control handling |
| `getrandom` | [#545](https://github.com/rrnewton/hermit/pull/545) | seeded bytes plus fault/flag coverage |
| scheduler affinity | [#546](https://github.com/rrnewton/hermit/pull/546) | fixed CPU0 mask and deterministic get/set policy |
| `writev` | [#547](https://github.com/rrnewton/hermit/pull/547) | fd-aware ordering and blocking-pipe progress |
| `madvise` | [#548](https://github.com/rrnewton/hermit/pull/548) | determinized advice policy across run, DBI, KVM, and R/R |

`epoll_ctl`, `eventfd2`, and `timerfd_create` received rationale and regression coverage in [#549](https://github.com/rrnewton/hermit/pull/549); that PR documented existing behavior rather than being counted as a ninth new handler area.

### `validate.sh` ratchet: 16 to 61

| Landed PR | Merge commit | Rows after merge |
| --- | --- | ---: |
| [#521](https://github.com/rrnewton/hermit/pull/521) | `0ad5ad5` | 16 |
| [#537](https://github.com/rrnewton/hermit/pull/537) | `4005bc4` | 32 |
| [#542](https://github.com/rrnewton/hermit/pull/542) | `d5b2a59` | 38 |
| [#550](https://github.com/rrnewton/hermit/pull/550) | `4968ad5` | 57 |
| [#554](https://github.com/rrnewton/hermit/pull/554) | `35ed10b` | 61 |

The row counts were measured directly from each merged version of `run_strict_compatibility_envelope`; they are not inferred from PR descriptions.

## Remaining blockers

| Blocker | Current state | Evidence | Exit criterion |
| --- | --- | --- | --- |
| Python/vfork scheduling | [Hermit PR #239](https://github.com/rrnewton/hermit/pull/239) is open, non-draft, `human-review`, and `locally-validated`; no review has been submitted. Portable CI passed; privileged failed because zlib development files were absent. | The exact site-wrapped Python probe on `2df293b` did not reach L2. The PR candidate passed 20/20 L2 repetitions with an empty read-only bind over `/var/run/nscd`; bare-host execution still diverged on live NSCD readiness. Backend ptrace, INFO log, relaxations none. | Human approval and merge; rerun the exact current-main Python probe both with controlled NSCD state and on the bare host, reporting external-state dependence separately. |
| DBI fork/process tree | [Reverie issue #31](https://github.com/rrnewton/reverie/issues/31) is open. It is an issue, not a PR. | `ppid()` remains `None`; correct clone/fork ancestry needs native DynamoRIO client tracking. The same issue also tracks precise timers and continuous clock reads. DBI matrix has 17 Run1 timeouts concentrated in pipelines and exec-oriented programs. | Implement native process-tree/lifecycle support and dispatch; rerun the current 61-row corpus under DBI L2. |
| KVM fork/clone | [Reverie issue #55](https://github.com/rrnewton/reverie/issues/55) is open. It is an issue, not a PR. | 19/26 KVM failures are direct `clone`/fork `ENOSYS`. Correct support needs child registers/address space, inherited descriptors, PID/TID identity, Detcore lifecycle callbacks, and deterministic scheduling. | Both issue reproductions and the 19 affected matrix rows pass KVM L2, with descriptor cleanup regressions. |
| R/R descriptor state | [Hermit PR #240](https://github.com/rrnewton/hermit/pull/240) is open, draft, and `human-review`; [issue #536](https://github.com/rrnewton/hermit/issues/536) tracks the `EpollWait` EOF hang. | 14 stdout-routing mismatches, five fd-number/order desyncs, and two toolchain replay timeouts. | Land the close fix after review, implement a real replay descriptor table for dup/open/fcntl/write routing, repair epoll lifecycle, then rerun 57 and 61 rows. |

## Recommended next steps

1. **Normalize the denominator.** After the current fixes land, rerun DBI, KVM, and R/R against the same 61 commands now on `main`; do not compare 20/38, 31/57, and 36/57 as if they measured identical coverage.
2. **Review and land PR #239.** It has the strongest immediate user impact and already has portable CI plus 20/20 isolated Python evidence. Preserve the NSCD caveat in the landing report.
3. **Review PR #240, then finish R/R fd tracking.** The existing close fix addresses one concrete cause, but numeric stdout injection and the descriptor table remain broader than `close(2)`.
4. **Implement KVM issue #55 before secondary loader work.** Fork/clone alone accounts for 19 of 26 KVM failures; then address `execve`, shebang loading, ELF layout, and filesystem metadata.
5. **Implement DBI native process lifecycle from issue #31.** Add clone/fork ancestry and lifecycle callbacks before interpreting pipeline timeouts as individual syscall bugs.
6. **Keep QEMU L2 opt-in but scheduled.** The gate is heavyweight and workflow-dispatch-only; run it after scheduler, procfs, signal, or blocking-I/O changes.

## Pull requests landed in the window

GitHub query: `merged:>=2026-07-24`, captured at 2026-07-24T09:18:27Z. Total: **39** = **32 Hermit + 7 Reverie**. The last merge in the dataset was Hermit #557 at 09:09:48Z.

### Hermit: 32

| PR | Merged UTC | Title |
| --- | --- | --- |
| [#260](https://github.com/rrnewton/hermit/pull/260) | 00:06:27 | Record SIOCETHTOOL as deterministic ENODEV |
| [#261](https://github.com/rrnewton/hermit/pull/261) | 00:14:46 | Fix record/replay for NULL getsockopt buffers |
| [#266](https://github.com/rrnewton/hermit/pull/266) | 01:01:25 | Add concurrency groups to CI workflows |
| [#269](https://github.com/rrnewton/hermit/pull/269) | 01:48:37 | Stabilize local validation gates |
| [#272](https://github.com/rrnewton/hermit/pull/272) | 02:04:11 | KVM M3D: Filesystem and multi-program support |
| [#274](https://github.com/rrnewton/hermit/pull/274) | 02:17:18 | waitid determinization |
| [#273](https://github.com/rrnewton/hermit/pull/273) | 02:24:48 | ppoll determinization |
| [#275](https://github.com/rrnewton/hermit/pull/275) | 02:56:03 | detcore: classify every x86_64 syscall explicitly |
| [#276](https://github.com/rrnewton/hermit/pull/276) | 03:08:25 | Set up merge queue |
| [#277](https://github.com/rrnewton/hermit/pull/277) | 03:40:57 | KVM: validate stdin flags end to end |
| [#503](https://github.com/rrnewton/hermit/pull/503) | 03:56:16 | detcore: promote 24 reviewed syscalls to pass-through |
| [#521](https://github.com/rrnewton/hermit/pull/521) | 04:03:17 | Add strict compatibility envelope to validation |
| [#533](https://github.com/rrnewton/hermit/pull/533) | 04:12:08 | Add targeted backend performance benchmarks |
| [#534](https://github.com/rrnewton/hermit/pull/534) | 04:19:58 | Determinize self prlimit64 handling |
| [#539](https://github.com/rrnewton/hermit/pull/539) | 04:45:01 | detcore: determinize arch_prctl controls |
| [#537](https://github.com/rrnewton/hermit/pull/537) | 04:55:04 | Expand strict compatibility application matrix |
| [#542](https://github.com/rrnewton/hermit/pull/542) | 05:29:33 | Expand strict command compatibility validation |
| [#545](https://github.com/rrnewton/hermit/pull/545) | 05:46:50 | Harden deterministic getrandom handling |
| [#546](https://github.com/rrnewton/hermit/pull/546) | 05:50:54 | Determinize scheduler affinity masks |
| [#543](https://github.com/rrnewton/hermit/pull/543) | 05:58:13 | Bump Reverie for DBI application syscall fix |
| [#541](https://github.com/rrnewton/hermit/pull/541) | 06:04:18 | Install zlib headers in privileged CI |
| [#547](https://github.com/rrnewton/hermit/pull/547) | 06:30:00 | Determinize writev syscall handling |
| [#329](https://github.com/rrnewton/hermit/pull/329) | 06:33:54 | Document strict QEMU boot syscall analysis |
| [#544](https://github.com/rrnewton/hermit/pull/544) | 06:39:35 | Restore KVM pipe and supplementary-group syscalls |
| [#548](https://github.com/rrnewton/hermit/pull/548) | 06:52:52 | Determinize madvise advice handling |
| [#549](https://github.com/rrnewton/hermit/pull/549) | 07:05:54 | Document and verify notification fd determinism |
| [#551](https://github.com/rrnewton/hermit/pull/551) | 07:50:04 | Allow strict direct recording from the CLI |
| [#550](https://github.com/rrnewton/hermit/pull/550) | 07:52:05 | validate: cover more strict system utilities |
| [#552](https://github.com/rrnewton/hermit/pull/552) | 07:55:19 | Read late ELF interpreters during replay |
| [#553](https://github.com/rrnewton/hermit/pull/553) | 08:24:53 | Add optional QEMU strict L2 validation gate |
| [#554](https://github.com/rrnewton/hermit/pull/554) | 08:52:59 | Validate complex developer tools under strict mode |
| [#557](https://github.com/rrnewton/hermit/pull/557) | 09:09:48 | Preserve SIGPIPE during record/replay |

### Reverie: 7

| PR | Merged UTC | Title |
| --- | --- | --- |
| [#48](https://github.com/rrnewton/reverie/pull/48) | 01:53:16 | DBI: support external Reverie tools |
| [#50](https://github.com/rrnewton/reverie/pull/50) | 02:04:48 | KVM M3D: Filesystem and multi-program runtime |
| [#49](https://github.com/rrnewton/reverie/pull/49) | 02:06:31 | Fix ppoll ABI |
| [#51](https://github.com/rrnewton/reverie/pull/51) | 03:07:43 | Merge queue setup |
| [#52](https://github.com/rrnewton/reverie/pull/52) | 03:12:24 | KVM: support fcntl F_GETFL |
| [#53](https://github.com/rrnewton/reverie/pull/53) | 05:16:57 | Pin DynamoRIO application syscall result fix |
| [#54](https://github.com/rrnewton/reverie/pull/54) | 06:27:30 | Support pipes and supplementary groups in KVM |

## Evidence index

| Evidence | SHA-256 |
| --- | --- |
| `/tmp/overnight-hermit-prs.json` | `6686c96da38265e5ffca832aacdfad38d4e7f09c83acffe740f487c5546cec1a` |
| `/tmp/overnight-reverie-prs.json` | `6ed9c315ae6b78700edd0332a8ebf5e94afd63042d4d8074185c13529ae3e0ee` |
| `/tmp/hermit-validate.7R7PLl.log` (current 61-row ptrace) | `3150547fca556847f3acca07d5efe9e72feeca00174bfa41924787fce579d724` |
| `/tmp/dbi-validate-full-console.log` | `beaed7ac47e8a958c6afcf08af48ee5dcbde7cdd4d089d76d1985e9dd87fac0a` |
| `/tmp/dbi-validate-full-detail.log` | `035c664996eb0969dc466bdc24cdb634068287bcf7d4727eb84a85850a14fa1a` |
| `/tmp/kvm-compat57-matrix.tsv` | `0ed8c5f3a37e3c94ec8340e211aed66709493be6d4959b68bf0bc2b6c117e7a1` |
| `/tmp/kvm-compat-57-slot87/hermit-validate.LEjhzw.log` | `7ba92f1304c73ad8556b552a3c3589a6e12a49d269e49df905ef9bcc8a6b6d5f` |
| `/tmp/hermit-rr-compat-13ea567.wCC2q8/report.md` | `7614a346b0234aae726dedcffff75686578e897b08552abdc3f45777106ba1b5` |
| `/tmp/hermit-rr-compat-13ea567.wCC2q8/matrix.tsv` | `6dcaeb8ddeb9e4be4fab55fb6ecee592ef57c9c6234718c670ece0b8e6a94010` |

Durable TaskGraph records: `impl-dbi-validate-full`, `impl-kvm-compat-57`, `impl-rr-compat-expansion`, `impl-fix-vfork-scheduling-race`, and `impl-test-complex-apps`.

## Caveats and non-claims

- The PR ledger is a UTC-window snapshot. Merges after 2026-07-24T09:18:27Z are intentionally absent.
- The 39 total counts merged pull requests only. Direct commits and issue closures are not included.
- Only ptrace 61/61 was rerun while writing this report. DBI, KVM, R/R, and QEMU numbers are exact archived measurements with their own SHAs and checksummed artifacts.
- The four programs added by #554 (`node`, `gcc`, `g++`, `make`) have not yet been folded into the DBI, KVM, or R/R denominator.
- QEMU L2 is ptrace-backed system-emulator execution, not evidence that Hermit's KVM backend can boot QEMU or Linux.
- DBI issue #31 and KVM issue #55 are tracking issues, not implementation PRs.
- `/tmp` artifacts are host-local and ephemeral; the key commands, totals, failure sets, SHAs, and hashes are therefore reproduced in this checked-in report.

## Bottom line

Fork `main` ends the window with a green, current 61-row ptrace L2 compatibility gate, an opt-in QEMU L2 boot gate, and materially broader syscall control. The next correctness work is concentrated rather than diffuse: human-review the vfork and replay-close patches, implement process lifecycle in DBI and KVM, then remeasure every mode on the same 61-row corpus.
