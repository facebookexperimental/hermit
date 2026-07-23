# Determinism experiments

This directory stores reproducible evidence about Hermit's observed behavior.
`run_experiment.sh` executes one command repeatedly under Hermit and records the
raw output and exit status from every run.

## Quick start

Build Hermit, then run a program at least twice:

```sh
cargo build -p hermit
./experiments/run_experiment.sh /bin/echo 10 hello
```

By default, the script uses `target/debug/hermit` and creates a timestamped
directory beside the script. Use explicit paths when collecting evidence for a
review:

```sh
./experiments/run_experiment.sh \
  --hermit ./target/debug/hermit \
  --output experiments/my_change_20260721/echo_fixed \
  /bin/echo 100 hello
```

The interface is:

```text
run_experiment.sh [--hermit PATH] [--hermit-log LEVEL] [--output DIR] PROGRAM RUNS [ARG ...]
```

`HERMIT_BIN` may set the default Hermit executable and `HERMIT_LOG_LEVEL` may
set the default log threshold. The script refuses to reuse an evidence
directory so an earlier observation cannot be overwritten.

## Observation model

Each run captures three values:

1. SHA-256 of raw stdout;
2. SHA-256 of raw guest stderr and Hermit diagnostics at the selected threshold;
3. process exit code returned by `hermit run`.

The default `error` threshold suppresses routine Hermit diagnostics whose
wall-clock timestamps would otherwise pollute the observation. Use
`--hermit-log` to select another threshold; keep it identical across compared
experiments. The script hashes the three captured values into one fingerprint.
It reports `DETERMINISTIC` only when every run has the same fingerprint. A
consistently nonzero exit code is deterministic under this definition; inspect
`runs.tsv` and the per-run files before interpreting the result as success.

The script exits with:

- `0` for `DETERMINISTIC`;
- `1` for `NON-DETERMINISTIC`;
- `2` for invalid arguments or setup failures.

## Evidence layout

An evidence directory contains:

```text
metadata.txt
runs.tsv
summary.txt
runs/
  run-0001/
    stdout
    stderr
    stdout.sha256
    stderr.sha256
    observation.txt
  run-0002/
    ...
```

`metadata.txt` records the repository commit, command, run count, and hashes of
the Hermit and guest executables. `runs.tsv` is the machine-readable manifest.
`summary.txt` contains the classification and number of unique fingerprints.
The stdout and stderr hash files use `sha256sum -c` compatible formatting when
run from their run directory.

Use descriptive, dated directories such as
`experiments/scheduler_change_20260721/`. Commit small textual evidence when it
supports a design or code review. Keep large binaries, traces, and core dumps
in external artifact storage and link them from a short Markdown summary.

## Methodology

For a meaningful branch comparison:

1. Build each branch with the same toolchain and profile.
2. Use the same Hermit log threshold; `error` is the recommended default for
   guest-observable determinism experiments.
3. Use the same guest executable, arguments, working tree contents, environment,
   host, and run count.
4. Record a baseline before the change and a candidate result afterward.
5. Compare `metadata.txt`, `summary.txt`, and `runs.tsv` before inspecting raw
   runs that differ.
6. Use enough iterations to exercise the relevant tail. Smoke tests may use
   5-10 runs; scheduling, PMU, signal, and race claims should generally use at
   least 100 and often 1,000 or more.

A matching fingerprint set is evidence of repeatability for the captured
observation, not proof that all internal scheduling or side effects matched.
Hermit does not make a changing filesystem or external network deterministic.
Stabilize those inputs or state the limitation in the experiment summary.
Likewise, a differing hash identifies an observed divergence but not its cause.

## Main baseline

`main_baseline_20260721/` records the initial smoke baseline from fork `main`.
It contains a fixed-output probe that should match and a procfs UUID probe that
demonstrates detection of a changing external filesystem input.
