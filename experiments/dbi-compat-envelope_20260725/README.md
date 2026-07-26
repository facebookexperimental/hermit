# DBI backend compatibility envelope (post-#713 DetConfig wiring)

**Date:** 2026-07-25
**Hermit:** `86e14633` (branch `dbi-sprint`, includes #713)
**reverie-dbi:** pinned rev `0e77d260` (consumed by `detcore-dbi` via git, not the local submodule)
**Backend:** `dbi` — the real `Detcore<DbiGuest>` path over DynamoRIO (not the old native PrototypeTool).

## Question

PR #713 wired the CLI `DetConfig` through to the DBI backend (serialized into the
`HERMIT_DBI_DETCONFIG` guest env var and deserialized by the `detcore-dbi`
cdylib), so `--backend dbi --strict` is no longer inert. How many real programs
now actually run — and run *deterministically* (L2) — under `hermit run --backend
dbi`, and where are the remaining backend-parity gaps versus the ptrace backend?

Before this measurement, the only committed DBI coverage was the 4-program
`experiments/backend-parity_20260722` matrix (echo/printf/true/cat). This
experiment shows the real envelope is far larger.

## Method

Three harnesses (in this directory), each program run with an isolated
`timeout --signal=KILL` because the measurement host was under heavy concurrent
load (30+ other `hermit` processes), which otherwise produces contention-induced
false `TIMEOUT`s that do **not** reproduce in isolation:

- `run_matrix.sh` — functional + native parity: guest stdout + exit under
  `hermit run --backend dbi` vs native.
- `l2_sweep.sh` — L2 determinism: `hermit run --backend dbi --strict --verify`
  (Hermit runs the guest twice and asserts bitwise-identical output +
  identical Detcore memory hash → "Determinism verified").
- `combined_matrix.sh` — the authoritative `results.csv`: for each program,
  native / ptrace / dbi(run) / dbi(--strict --verify L2).
- `clockprobe.c` — vDSO vs forced-raw-syscall `clock_gettime`, to localize the
  clock gap.

Reproduce (from a hermit checkout with the release binary built):

```bash
cargo build --release -p hermit --bin hermit
# Fresh worktree seeded from another target/ dir: clear the stale cmake cache once
rm -rf target/release/**/detcore-dbi-native-*
HERMIT=target/release/hermit bash experiments/dbi-compat-envelope_20260725/combined_matrix.sh
```

## Results

**Functional (`run --backend dbi`, vs native stdout+exit): 33/35.**
echo, true, false, printf, pwd, whoami, id, hostname, arch, seq, cat, head,
tail, wc, sort, uniq, cut, tr, rev, base64, sha256sum, cksum, od, nl, tac,
sleep, `sh -c`, `bash -c` (echo / pipe / loop), `python3 -c`, `perl -e` all pass.
The only non-matches are `uname` and `date` (see gaps) plus `nproc` (a *false*
mismatch — see below).

**L2 determinism (`run --backend dbi --strict --verify`): 22/23 "Determinism
verified"** across the corpus below (the lone exception was a contention TIMEOUT
that passes in isolation). See `results.csv` for the authoritative per-program
table (`dbi_L2_verify` column).

`nproc` reports `1` under **both** dbi and ptrace (Hermit virtualizes the CPU
count); the raw-vs-native diff was misleading — dbi/ptrace **parity is correct**.

## Gaps (both precisely root-caused)

### GAP 1 — clock/`date` returns 1970 (#705): a reverie-dbi vDSO bug

`clockprobe` under `--backend dbi`:

```
vdso_sec=1   raw_syscall_sec=1640995199
```

- The **forced raw** `clock_gettime(CLOCK_REALTIME)` syscall is correctly
  virtualized by Detcore to the 2021 epoch (`1640995199` =
  `2021-12-31T23:59:59Z`, the `DEFAULT_EPOCH_STR`). Detcore's DBI time path works.
- The **vDSO fast path** returns ~0 (→ 1970), so glibc callers (`date`,
  `python time`, etc.) get an un-virtualized clock. ptrace returns 2021 for
  *both* paths because reverie-ptrace neutralizes the vDSO.

**Root cause (localized to `reverie-dbi/native/client.c`, pinned rev `0e77d260`):**
the client wraps `__vdso_clock_gettime` / `__vdso_getres` / `__vdso_gettimeofday`
/ `__vdso_time` (see `wrap_vdso_symbol`) and routes them to the *native*
`handle_virtual_clock`, which reads the prototype's own `virtual_time_ns` atomic
counter (base 0) — **not** Detcore's `GlobalTime` (seeded from `cfg.epoch`). These
vDSO wrappers predate the real-Detcore integration; the trapped-syscall path
(`reverie_dbi_runtime_pre_syscall` → real Detcore) is correct, but the vDSO
short-circuits it.

**Fix locus (reverie-dbi, NOT hermit-only):** either route the vDSO wrappers
through `reverie_dbi_runtime_pre_syscall` (so they return Detcore time), or drop
the wrappers and neutralize the guest vDSO so glibc falls back to the trapped
syscall. Because `detcore-dbi` consumes `reverie-dbi` at a pinned git rev, this
requires a coordinated reverie change + re-pin, out of this Hermit-only task's
scope.

### GAP 2 — container identity (uid / user / hostname / uname nodename) not applied under DBI

DBI presents the **real host identity**, matching native but diverging from
ptrace's virtualized container identity (measured directly):

| program    | native            | ptrace                     | dbi               |
|------------|-------------------|----------------------------|-------------------|
| `id -u`    | 212630            | **0**                      | 212630            |
| `whoami`   | newton            | **root**                   | newton            |
| `hostname` | devbig030…        | **hermetic-container.local** | devbig030…      |
| `uname` nodename | devbig030…  | **hermetic-container.local** | devbig030…      |

The `uname` kernel-version field *is* virtualized under dbi (`5.2.0`, matching
ptrace) because that comes from the `uname` **syscall** Detcore rewrites; the
nodename is not, because it derives from the UTS namespace, not the syscall.
Note this means DBI's high native-match rate is partly a consequence of applying
*less* isolation than ptrace — these values are still deterministic (stable
across runs → L2 passes), but they are cross-backend parity gaps.

**Root cause:** DBI is dispatched *before* the container machinery —
`hermit-cli/src/bin/hermit/run.rs:1404-1420` calls `super::backends::run_dbi(...)`
and returns, bypassing `default_container` / `with_container` /
`identity_hardening_mounts`. So the UTS namespace (hostname), frozen `/etc/group`,
hidden nscd, and `/proc` isolation that ptrace applies are absent under DBI.

**Fix locus (hermit-cli, in scope but non-trivial):** launch `drrun` inside the
Hermit container namespaces. Deferred as a separate change; risk of destabilizing
the working DBI baseline and cannot be fully CI-validated on this loaded host.

## Interpretation

Post-#713, the DBI backend runs a broad corpus of real system/text/shell/
interpreter programs and is **L2-deterministic** for essentially all of them —
far beyond the 4-program committed matrix. The two remaining parity gaps are both
localized to a single file each: the clock gap is a **reverie-dbi** vDSO-routing
bug (#705), and the identity gap is a **hermit-cli** container-bypass. Neither
affects determinism *under DBI* — `date` is deterministically 1970, so it still
passes DBI's own `--strict --verify`; the gaps are cross-backend *parity*, not
DBI nondeterminism.

## Files

- `metadata.json` — SHAs, host, toolchain, commands.
- `results.csv` — authoritative per-program native/ptrace/dbi-run/dbi-L2 table.
- `run_matrix.sh`, `l2_sweep.sh`, `combined_matrix.sh`, `clockprobe.c` — harnesses.
