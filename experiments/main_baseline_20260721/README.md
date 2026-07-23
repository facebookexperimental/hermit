# Main baseline: 2026-07-21

This smoke baseline was collected from `main` commit
`3f3c31c45b1d6a750b716bc3efd96efb2a575e76` on an x86-64 host with an AMD
EPYC 9D85 158-Core Processor. Both probes used the same debug Hermit binary and
Hermit's `error` log threshold.

## Fixed output

```sh
./experiments/run_experiment.sh \
  --hermit ./target/debug/hermit \
  --output experiments/main_baseline_20260721/echo_fixed \
  /bin/echo 5 deterministic-baseline
```

Result: `DETERMINISTIC`. All five runs exited 0 and produced fingerprint
`e12f6b880e932e1247d425ef75ad7878c632f09341ad9a8258fd0286e9ff79f4`.

## Changing procfs input

```sh
./experiments/run_experiment.sh \
  --hermit ./target/debug/hermit \
  --output experiments/main_baseline_20260721/proc_random_uuid \
  /bin/cat 5 /proc/sys/kernel/random/uuid
```

Result: `NON-DETERMINISTIC`. All five runs exited 0, but each observed a
unique stdout and composite fingerprint. This is expected because the procfs
UUID is an external changing input; the probe verifies that the runner detects
an observable divergence rather than asserting a Hermit defect.

These five-run probes validate the evidence pipeline. They are not a
statistical determinism claim; scheduler, PMU, signal, and race experiments
should use the larger run counts described in the parent methodology.
