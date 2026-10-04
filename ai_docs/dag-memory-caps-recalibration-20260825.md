# Portable DAG memory-cap recalibration (2026-08-25)

## Finding

Fourteen portable DAG steps carried the identical pair
`rss_baseline_bytes=8589934592` (8 GiB) and
`hard_mem_max_bytes=17179869184` (16 GiB). They were not fourteen independent
measurements. Commit `a6201fc65d29c3a1a88cc7af4b117b68e9950284` raised them together as a
deliberately generous safety blanket. Twelve were unmeasured at that point;
`lint.clippy` and `test.hermit_unit` had lower measurements that the blanket
overwrote.

Existing cgroup profiles prove that all fourteen declarations were stale in the
same direction: every completed historical high-water mark is below the 8 GiB
baseline, and none supports a 16 GiB hard cap. This is over-declaration, not
under-declaration.

## Current-command measurement

The exact commands from `ci/dag/validate.json` were run in persistent user
systemd scopes at Hermit `58725c5778b631353ef2aff0326c187a065d7332`, with
`MemoryAccounting=1`, `MemoryMax=16GiB`, `MemorySwapMax=0`, and
`CARGO_BUILD_JOBS=8`. The scoped commands are unchanged through the landing
base. At least five successful, uncensored samples were collected for thirteen
steps; `test.app_strict_verify` required one replacement after an unrelated
failed/censored attempt. The table reports their largest value, which is also
the p90 with five samples, and the repository memory-feedback policy's p90 plus
20%.

`test.hermit_integration` completed its full 98-test population but was red in
all five repetitions for existing test failures. Its peaks are therefore
censored and cannot justify a lower estimate. Its last successful historical
peak and an older 4 GiB OOM supply the conservative floor instead.

| Step | Current successful samples | Current p90/max | p90 + 20% | Historical completed max | New baseline / hard cap |
|---|---:|---:|---:|---:|---:|
| `build.liteinst_runtime_release` | 5 | 0.287 GiB | 0.345 GiB | 3.862 GiB | 4 / 6 GiB |
| `lint.clippy` | 5 | 0.260 GiB | 0.312 GiB | 4.0 GiB at build width 8 | 4 / 6 GiB |
| `test.hermit_unit` | 5 | 1.053 GiB | 1.264 GiB | 4.6 GiB at build width 8 | 5 / 7 GiB |
| `test.detcore_misc` | 5 | 0.062 GiB | 0.075 GiB | 1.581 GiB | 2 / 4 GiB |
| `test.detcore_parallel` | 5 | 0.383 GiB | 0.460 GiB | 1.495 GiB | 2 / 4 GiB |
| `test.hermit_integration` | 0 (5 censored) | n/a | n/a | 4.984 GiB; 4 GiB previously OOMed | 6 / 8 GiB |
| `test.arbitrary_binaries` | 5 | 0.238 GiB | 0.285 GiB | 1.803 GiB | 2 / 4 GiB |
| `test.cli` | 5 | 0.316 GiB | 0.379 GiB | 3.789 GiB | 4 / 6 GiB |
| `test.hermit_modes` | 5 | 0.267 GiB | 0.321 GiB | 1.899 GiB | 2 / 4 GiB |
| `test.app_strict_verify` | 5 successful (+1 censored) | 1.399 GiB | 1.679 GiB | 2.643 GiB; 4 GiB previously pressured | 3 / 6 GiB |
| `test.command_strict_verify` | 5 | 0.223 GiB | 0.268 GiB | 1.815 GiB | 2 / 4 GiB |
| `test.ignored_syscall_regressions` | 5 | 0.250 GiB | 0.300 GiB | 1.653 GiB | 2 / 4 GiB |
| `test.dbt_parity` (formerly `test.dbi_parity`) | 5 | 0.078 GiB | 0.094 GiB | 0.185 GiB | 0.5 / 2 GiB |
| `test.envelope_levels` | 5 | 0.027 GiB | 0.033 GiB | 0.057 GiB | 0.125 / 1 GiB |

The authored baseline is the larger of the current p90-plus-margin result and
the historical completed high-water, rounded upward to an operationally simple
boundary. The hard cap adds at least 2 GiB for the larger steps and never falls
below the historical adverse evidence. Thus warm cache state may not erase a
cold historical peak, and a failed/censored run may not lower a cap.

## Evidence locations

- Current exact-command results:
  `experiments/portable-memory-14-20260825/results-current-six.csv` in the parent
  experiment workspace. It retains the failed/censored application attempt and
  its successful replacement instead of silently dropping either.
- Width-8 calibration for Clippy and Hermit unit tests:
  `experiments/dag-mem-caps-pinned-jobs_20260804/results.csv`.
- Historical profile maxima are retained in the corresponding
  `.dagrun/profiles/step_profiles*.csv` stores. The largest
  integration sample is 5,351,632,896 bytes; the largest LiteInst build sample
  is 4,147,245,056 bytes; and the largest CLI sample is 4,068,401,152 bytes.

The step set, commands, dependencies, timeouts, resource locks, and cell
population are unchanged by this recalibration.

## Proposed-cap validation

At base `50794c05b1a020240759d6afeaa9f14ee5ba8f29`, each exact command
was run once more with its proposed `hard_mem_max_bytes` as the actual cgroup
`MemoryMax`. All thirteen previously green commands completed successfully.
`test.hermit_integration` again completed the full population and returned its
existing test-failure status, not an OOM or cgroup kill; it peaked at
1,667,670,016 bytes under the 8 GiB cap. The largest successful proposed-cap
runs were `test.hermit_unit` at 3,977,146,368 bytes under 7 GiB,
`test.detcore_misc` at 2,085,179,392 bytes under 4 GiB, and `lint.clippy` at
2,023,055,360 bytes under 6 GiB.
