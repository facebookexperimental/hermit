# Record/replay hardening follow-up: 2026-07-21

This follow-up retests the Python, GCC, and curl cases that were missing or
timed out in `record-replay-matrix_20260721`. It also validates Hermit's new
internal recording deadline. Tests ran on Linux 6.13.2, x86-64, from fork
`main` commit `3a0ac002356642913c886de6d3b004fd4bd55c51` plus this change.

## Results

| Program | Record | Replay | Observation |
| --- | --- | --- | --- |
| `python3 -c` | controlled timeout | not run | Python enters native `vfork`; the parent blocks before Detcore registers the child, and the child waits for a scheduler entry the parent cannot create. `--record-timeout=2` returns at the two-second deadline, removes partial data, and leaves no guest process. |
| `gcc -c -O0` | pass | diverges | An isolated current-main recording completes promptly. Replay diverges after `cc1` execs: thread 5 executes `write(4, ..., 16)` where the recording expects `brk(NULL)`. |
| `curl --version` | pass | pass | Record and replay complete in about four seconds with identical output and exit status. This case is now an integration test. |
| spinning shell | controlled timeout | not run | `--record-timeout=1` kills the guest, returns promptly, and does not update the recording store's `last` pointer. This case is now an integration test. |

The deadline uses a process alarm inside the isolated recording container, so it does not consume a guest-visible PID or TID.

The earlier matrix's GCC *record* timeout did not reproduce on current `main`.
The old runner wrapped the outer CLI with GNU `timeout`, which can leave the
recording container and ptrace tracees behind. Those leaked processes can
contaminate later serial cases, so the historical GCC result should not be
treated as a current compiler-recording failure. GCC replay remains a real,
separately reproduced failure.

## Commands

Build the candidate:

```sh
cargo build -p hermit --bin hermit
```

Exercise the internal timeout against the Python failure:

```sh
HERMIT_MODE=record target/debug/hermit --log off record start \
  --record-timeout=2 --data-dir=/tmp/hermit-python-record -- \
  /usr/local/bin/python3 -c \
  'import hashlib; print(hashlib.sha256(b"hermit").hexdigest())'
```

Run the committed curl and deadline regressions:

```sh
cargo test -p hermit --test record_replay record_curl_version
cargo test -p hermit --test record_replay \
  record_timeout_kills_guest_without_committing_partial_data
```

The GCC probe used:

```sh
HERMIT_MODE=record target/debug/hermit --log off record start \
  --record-timeout=30 --data-dir=/tmp/hermit-gcc-record -- \
  /usr/bin/gcc -c -O0 \
  experiments/record-replay-matrix_20260721/fixtures/compile_input.c \
  -o /dev/null
```

## Remaining work

1. Land child-side registration for native `clone`/`vfork` before classifying
   Python as supported. Draft PR #27 demonstrates that this unblocks the tested
   Python workload, but its review blockers remain prerequisites.
2. Diagnose the GCC `cc1` syscall-order divergence using the generated replay
   context and desynchronization report.
3. Make replay failure cleanup reliably reap every tracee. The richer replay
   diagnostic is emitted immediately, but the outer task tree can still wait
   on tracees after a mismatch.

This is targeted failure-mode evidence, not a broad compatibility claim. The
original matrix remains the baseline for the other workloads.
