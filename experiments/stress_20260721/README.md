# Speculative branch stress evidence: 2026-07-21

## Scope

This evidence was collected from speculative commit
`96261f618dda654fb87ffacd0e178c4bf743faaf` on an x86-64 AMD EPYC 9D85 host.
The workspace build passed before testing:

```sh
cargo build --workspace
```

The experiment runner came from draft PR #43 because that later infrastructure
commit is not an ancestor of the speculative aggregate. No speculative source
commit was created. The tested Hermit binary has SHA-256:

```text
50c7ec20341311b510637216f912cbd7601efc475443cd5c6713a99d05d825cd
```

A runner result of `DETERMINISTIC` only means stdout, stderr, and exit status
matched. A repeated nonzero status is therefore listed as an operational failure
below even when its fingerprint is stable.

## Repeated execution matrix

| Workload | Runs | Exit status | Fingerprints | Result |
| --- | ---: | ---: | ---: | --- |
| `/bin/echo speculative-determinism` | 25 | 0 | 1 | PASS |
| `/bin/ls -1 tests/c` | 25 | 0 | 1 | PASS |
| Python SHA-256 of 1 MB | 25 | 0 | 1 | PASS |
| C `nanosleep-par` pthread guest | 10 | 0 | 1 | PASS |
| C `printf_with_threads` pthread guest | 25 | 134 | 1 | FAIL: consistent SIGABRT |
| C `just_spin` pthread guest | 10 | 134 | 1 | FAIL: consistent SIGABRT |
| QEMU Linux boot, strict defaults | 3 | 134 | 1 | FAIL: aborts before boot output |
| QEMU Linux boot, compatibility mode | 3 | 124 | 3 | FAIL: output differs and boot needs timeout |

The real-program commands used the following form; each named evidence directory
contains `metadata.txt`, `runs.tsv`, per-run raw output, hashes, and a summary:

```sh
./experiments/run_experiment.sh --hermit ./target/debug/hermit \
  --output experiments/stress_20260721/echo \
  /bin/echo 25 speculative-determinism

./experiments/run_experiment.sh --hermit ./target/debug/hermit \
  --output experiments/stress_20260721/ls \
  /bin/ls 25 -1 tests/c

./experiments/run_experiment.sh --hermit ./target/debug/hermit \
  --output experiments/stress_20260721/python_hash \
  /usr/bin/python3 25 -c \
  'import hashlib; print(hashlib.sha256(b"x" * 1000000).hexdigest())'
```

The C guests were built from tracked sources:

```sh
gcc -O2 -pthread tests/c/printf_with_threads.c \
  -o target/stress-guests/printf_with_threads
gcc -O2 -pthread tests/c/just_spin.c \
  -o target/stress-guests/just_spin
gcc -O2 -pthread tests/c/nanosleep-par.c \
  -o target/stress-guests/nanosleep_par
```

## Internal determinism verification

Passing and failing representatives were also run with Hermit's two-run verifier
and memory-map hashing enabled:

```sh
./target/debug/hermit run --verify \
  --detlog-heap --detlog-stack --preemption-timeout=disabled -- PROGRAM ARGS...
```

| Workload | Messages per run | Detlog/scheduler entries | Log comparison | Operational result |
| --- | ---: | ---: | --- | --- |
| echo | 1,048 | 771 | no substantive differences | PASS, exit 0 |
| ls | 1,404 | 1,055 | no substantive differences | PASS, exit 0 |
| Python SHA-256 | 10,907 | 8,871 | no substantive differences | PASS, exit 0 |
| C nanosleep threads | 1,178 | 665 | no substantive differences | PASS, exit 0 |
| C printf threads | 530 | 376 | no substantive differences | FAIL, exit 134 |

The verifier compares guest stdout/stderr/status plus debug detlogs containing
syscall, scheduler, and requested heap/stack memory hashes. The raw verifier
summaries are under `verify/`.

Do not pass global `--log error` to this verifier: a control run showed that it
then compares zero internal messages while still printing a success result.
The table above was regenerated without that threshold and contains nonzero
trace counts.

## Integrated matrices

The speculative arbitrary real-binary matrix passed:

```sh
cargo test -p hermit --test arbitrary_binaries \
  run_arbitrary_binary_matrix -- --exact --nocapture
```

The stable real-binary record/replay matrix also passed:

```sh
cargo test -p hermit --test arbitrary_binaries \
  record_replay_stable_arbitrary_binaries -- --exact --nocapture
```

The new fast chaos matrix failed on its first case, before exercising the rest
of its 320-case matrix:

```sh
cargo test -p hermit --test stress_suite \
  fast_chaos_matrix -- --ignored --exact --nocapture
```

Failure: `atomic-lost-update`, 2 threads, seed 0 terminated with `SIGABRT` after
`The futex facility returned an unexpected error code.` The slower 100-seed and
PMU tiers were not run because they use the same blocked concurrency path.

## QEMU Linux boot

Inputs:

```text
QEMU 10.1.0
qemu-system-x86_64: ea7cb62100804058a4e97fca129c77b77487a3d175d04551466b4e44f094f3ca
vmlinuz:             ce6aae1633026f6d43fe01ddb847247b8a522aaf1f6a612a8d2c49e3c43b22c0
kdump initramfs:      7d34ba34b3299987ba7e5418e8c027f716f036d82653e22961a2ccd9346e06a8
```

The strict command used TCG, one virtual CPU, 256 MB RAM, serial output, and the
host kernel plus readable kdump initramfs. All three runner attempts aborted
with status 134 before producing kernel stdout. Informational diagnosis and the
independent pthread/chaos failures point to the speculative futex/resource-model
integration; the outcome is sensitive to logging/timing and is not a usable
strict boot.

`hermit_compat_adapter.sh` reruns the same command with:

```text
--no-sequentialize-threads --no-deterministic-io
--preemption-timeout=disabled
```

and a 30-second host timeout. All three compatibility runs reached Linux
`/init`; the kdump initramfs then exited because its crash-kernel parameter is
absent, producing the expected kernel panic. However, each run needed the host
timeout (124) and all three output hashes differed. The first output divergence
starts at kernel line 106 and includes virtual kernel timestamps, crypto/RAID
benchmark rates, and later stack values. This mode therefore demonstrates that
the virtual-time boot path executes, but it does not provide deterministic boot
evidence.

A compatibility-mode `hermit run --verify` attempt was bounded at 240 seconds.
It printed only `:: Run1...` and timed out before completing the first recorded
run, so no QEMU internal-log comparison was obtained.

## Conclusion

The speculative branch is deterministic for the tested single-process tools,
Python CPU workload, one pthread sleep workload, and the stable arbitrary-binary
record/replay set. It is not ready to claim systematic multi-thread or QEMU
determinism: common pthread joins abort, the integrated chaos suite cannot start,
strict QEMU aborts, and compatibility QEMU output diverges across runs.
