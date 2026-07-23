---
name: progress-rubric
description: "Create evidence-based Hermit progress reports from live measurements. Use when gathering, validating, or writing a project progress report."
---

# Progress Report Rubric

Use this procedure for dated reports in `ai_docs/progress-reports/YYYY-MM-DD.md`.
The primary question is not how many tests exist; it is **which real programs
work in each execution mode, and where support drops as users move from the
leading mode to trailing modes**.

## Non-negotiable evidence rules

1. Measure one exact `origin/main` SHA in a clean checkout. If main moves and
   product code changed, update and rerun affected measurements.
2. Run the same app probes through `--strict --verify`, record/replay, DBI, and
   KVM. Do not substitute old task notes for live results.
3. Use only code and artifacts available from main. Unlanded work belongs in a
   final footnote, never in the coverage totals.
4. Mark unavailable or interrupted measurements honestly. `BLOCKED`, `NOT RUN`,
   and `INCOMPLETE` are not failures, but none count as passes.
5. State backend, log level, relaxations, command, exit status, and material
   output for every Hermit measurement.

## Required report shape

1. **Snapshot**: full SHA, UTC time, host/kernel, PMU policy, toolchain, and
   guest binaries used.
2. **Coverage slope**: one compact row per mode showing `passed / probes`, the
   first unsupported workload class, and the current blocking layer.
3. **App matrix**: one row per identical program/workload, columns for strict
   verify, R/R, DBI, and KVM. Include at least a trivial ELF, a file-processing
   tool, an interpreter, a concurrent pipeline, and a toolchain frontend.
4. **Repository health**: `cargo test`, `validate.sh`, the working-envelope
   vector, focused R/R suite, and live main CI. Preserve incomplete-run status.
5. **Gaps and next actions**: order by the first mode where support drops.
6. **Unlanded footnote**: at most three bullets with links; no unlanded result
   may alter a main-branch cell.

## Measurement procedure

### Freeze main and context

```bash
with-proxy git fetch origin main
git switch -c report-YYYY-MM-DD origin/main   # in a clean assigned worktree
git rev-parse HEAD
date -u +%Y-%m-%dT%H:%M:%SZ
uname -r
grep -m1 'model name' /proc/cpuinfo
cat /proc/sys/kernel/perf_event_paranoid
rustc --version; cargo --version; cargo nextest --version
cargo build -p hermit
```

Re-fetch before writing. If only docs moved, record that fact; if product code
moved, rerun the affected cells at the new SHA.

### Cross-mode app matrix

Choose a small, stable probe list and keep it identical across modes. A useful
minimum is:

```text
/bin/echo hello
/usr/bin/sha256sum /etc/hostname
/usr/bin/python3 -c 'print(sum(range(100)))'
/bin/bash -c '/bin/echo hello | /usr/bin/wc -c'
/usr/bin/gcc --version
```

For each probe, run:

```bash
./target/debug/hermit run --strict --verify -- PROGRAM ARGS...
./target/debug/hermit record start --verify --record-timeout 90 \
  --data-dir "$(mktemp -d /tmp/hermit-report-rr.XXXXXX)" -- PROGRAM ARGS...
./target/debug/hermit run --backend dbi -- PROGRAM ARGS...
./target/debug/hermit run --backend kvm -- PROGRAM ARGS...
```

If DBI or KVM has a backend-wide preflight failure, run one representative
probe, quote the error, and mark the remaining cells `BLOCKED*` with one shared
footnote. Do not use a client, SDK, pin, or proof branch that is not supplied by
main. For R/R, distinguish record timeout, replay divergence, output mismatch,
and successful round trip.

### Repository health

```bash
cargo test 2>&1 | tee /tmp/progress-cargo-test.log
cargo test -p hermit --test record_replay -- --test-threads=1
./validate.sh 2>&1 | tee /tmp/progress-validate.log
ENVELOPE_JSON=/tmp/progress-envelope.json ./validate.sh --envelope-only
with-proxy gh run list -R rrnewton/hermit --branch main --limit 6
```

Bound commands that can hang and report the bound. Aggregate only completed
`test result:` lines. If a process is killed or interrupted, name the last
completed step and state that no final summary was produced. The `rr_suite`
target may exist but have ignored PMU/mount-namespace cases; report executed and
ignored counts separately.

## Cell vocabulary

- `PASS L2`: strict verify completed and reported deterministic output.
- `PASS R/R`: record completed, replay completed, and outputs/logs matched.
- `FAIL`: the mode ran the guest and produced a mismatch, divergence, crash, or
  nonzero result attributable to that workload.
- `BLOCKED`: a backend-wide dependency or implementation gate prevented guest
  execution; quote it once.
- `NOT RUN`: no command was attempted. State why.

## Accounting and writing rules

- Use one denominator for the same-app matrix. Broader historical matrices may
  be linked as context but must not be added to fresh probe counts.
- Present the support slope explicitly, for example `5/5 -> 4/5 -> blocked ->
  blocked`, then explain the first lost workload.
- Separate product gaps from changing host state, missing privileges, and test
  harness interruption. Say `unknown` when attribution is not established.
- Never call main green unless the exact-SHA gate completed successfully.
- Do not stage generated logs, recordings, `envelope.json`, `target/`, or
  unrelated concurrent changes.
