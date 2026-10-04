# `ppoll-simulation` SaBRe remeasurement on 2026-08-27

## Result

`c-programs/ppoll-simulation/verify/sabre` still diverges on Hermit
`d42c3f52b4c3298187bb0e2c754229789be1478a`, with Reverie pinned at
`1393db6279331feb4af2533220e4786ab104a1b9`.

The sample at that Hermit SHA, which was Hermit main on 2026-08-27, reproduced
13 divergent strict comparison pairs:

| Population | Solo preflight diverged | Measured pairs | Diverged | Matched | Wall time |
| --- | ---: | ---: | ---: | ---: | ---: |
| concurrent, `-j 4` | 0 of 1 | 59 | 8 | 51 | 17.8 s |
| sequential, `-j 1` | 1 of 1 | 59 | 4 | 55 | 88.6 s |

The populations are reported separately because concurrent runs include host
contention. These counts establish that the divergence reproduces at this SHA.
They are not a claim that the two observed percentages differ.

All 13 divergences had the same first differing INFO record: record 118,
scheduler turn 110, committing `Device(ContainerStdout): W`. The resource and
control flow at that record matched; only virtual time differed, by either
5,000 or 10,000 nanoseconds. Under `docs/DIVERGENCE_CLASSES.md`, that makes the
first divergence `pure-clock`.

The later syscall context identifies the cause more specifically. In every one
of the 13 divergent pairs, both executions had completed the delayed writer's
`clock_nanosleep` and `write`, followed by the main thread's successful
`ppoll` syscall 22. At syscall 23, one execution entered a `futex` from
`pthread_join`, while the other proceeded to the `fstat` issued on the stdout
path. The direction varied between pairs. For example:

```text
first difference:
  run 1: COMMIT turn 110 ... Device(ContainerStdout): W ... 1767225600.031999250s
  run 2: COMMIT turn 110 ... Device(ContainerStdout): W ... 1767225600.031994250s

later syscall context:
  run 1: finish syscall #23: futex(..., 265, 6, NULL, NULL, -1) = Err(EAGAIN)
  run 2: finish syscall #23: fstat(1, ...) = Ok(0)

shared preceding calls:
  writer: clock_nanosleep(...) = Ok(0)
  writer: write(...) = Ok(1)
  main:   finish syscall #22: ppoll(..., 1, NULL, NULL, 8) = Ok(1)
```

This is the same pthread exit/join timing mechanism as the other SaBRe cells:
the main thread sometimes observes that the delayed writer has fully exited
before `pthread_join`, and sometimes enters the join futex. The source performs
that sequence in `tests/c/ppoll_simulation.c`: create the delayed writer, block
in `ppoll`, then call `pthread_join` before printing success.

The historical movement from record 118 to record 198 says nothing about the
cause. Record numbers include however much trace precedes an event, so the same
event can move to another record number. The stable identities here are the
event content and scheduler/syscall context: the first difference is the
virtual time on scheduler turn 110, and the later split is the main thread's
syscall 23 after `ppoll`. The timing-dependent conclusion comes from that
content, not from coordinate movement. This refutes using record-number drift
to identify the cause.

## Relationship to the landed host wait change

The remeasurement includes the work from
https://github.com/rrnewton/reverie/pull/506 and
https://github.com/rrnewton/hermit/pull/2737. It did not resolve this cell.

That result is consistent with the code boundary rather than evidence that the
wait change failed. Hermit sets
`backend_requires_thread_directed_process_signals` only for DBT; SaBRe leaves
it false. The changed path handles process-child waits such as `wait4`, while
this guest creates a pthread and reaches the split after `ppoll` at
`pthread_join`. The ppoll result is therefore separate from the host-timed
`wait4` defect.

## SaBRe execution-path limitation

One exact pressure-test pair also ran. Its strict report matched with
`bitwise_parity: true`, 295 INFO messages on each side, 112 scheduler turns,
79 syscalls, and 33,254,250 virtual nanoseconds. The pressure harness correctly
did not count it as a passing cell because both executions recorded:

```json
{"schema":1,"guest_rpc_observed":true,"ptrace_fallback_sites":0,"trusted_shared_object_sites":1,"trusted_shared_objects":["/usr/lib64/libc.so.6"]}
```

The pressure result is therefore `crash-error`, with reason `SaBRe execution
path is incomplete or used fallback/native sites`. It is not a passing SaBRe
cell result. The repeat tool records strict comparison results but does not
emit this separate SaBRe path evidence, so the repeated pairs establish
reproduction and the divergence location; they do not qualify the scorecard
cell. No scorecard status was changed.

## Commands

From a `dev-hermit` checkout whose `hermit/` worktree is at the stated SHA:

```bash
cd hermit
./ci/compat-envelope/pressure-test.rs run \
  --results ignored/ppoll-sabre-exact \
  --test c-programs/ppoll-simulation \
  --mode verify --backend sabre --cell-timeout 60 --jobs 1

cd ..
bin/hermit-repeat \
  --cell c-programs/ppoll-simulation --cell-backend sabre \
  -n 59 --mode concurrent -j 4 --deadline 60 --keep-artifacts \
  --max-total-mb 3072 --out hermit/ignored/ppoll-sabre-concurrent \
  --json hermit/ignored/ppoll-sabre-concurrent.json

bin/hermit-repeat \
  --cell c-programs/ppoll-simulation --cell-backend sabre \
  -n 59 --mode sequential --deadline 60 --keep-artifacts \
  --max-total-mb 3072 --out hermit/ignored/ppoll-sabre-sequential \
  --json hermit/ignored/ppoll-sabre-sequential.json
```

The run used Linux `6.13.2-0_fbk17_hardened_0_g2ae417e0caa0`, x86-64,
an AMD EPYC 9D85 host with 316 logical CPUs, `perf_event_paranoid=1`, and
`rustc 1.99.0-nightly (26ae60a9e 2026-07-28)`.

## Retained evidence checksums

The bulk logs remain ignored, while the findings and representative records
above are retained here. These hashes identify the exact local inputs used:

| File | SHA-256 |
| --- | --- |
| exact pressure summary | `d2abd474f2b6d09783c0b1e9fd9498fb80766f8b064dc2d5af8b93951da52d34` |
| exact strict report | `5a38c2c52b3ffa48570c88ca276657c9db219fbcaab85c86e9e8b1b48e07e6b0` |
| exact SaBRe path evidence | `c495b70d77965edeef385fe569a1f2674fe97da76db3d17bef7d40c0d087c6f1` |
| concurrent 59-run JSON | `d7c5e10010ecac93b5439e6fd2cb64786541e1f9ffc28cb39cbdbf15e5cb364b` |
| sequential 59-run JSON | `8623ee63f5dcb39c06d28e4f8488086d41d4c24b7d6edc3a1341ce29470f7453` |
| representative concurrent divergence log | `336665019ceb62bcd0e48d9fb8ecb2184db9e06ad1f3ebcc73ba8ec2b1780418` |
| sequential preflight divergence log | `2405da3e28fa3bc3f57f3ab4ce6a78cf9e4ffebcfc7c8a6be136906805c1a62d` |

An earlier exact-SHA sample at Hermit
`1540f91a0539e0cec8923d33220cdc316c910a0b` had 0 divergences in 10 strict
pairs. That means only that the divergence was not reproduced in that sample;
the populations above, at Hermit `d42c3f52`, reproduce it directly.

## What tree this measures, and how to obtain it

⚠️ **The Reverie SHA above is a pull-request head, and it is NOT REACHABLE FROM
`reverie/main`.** A fresh checkout cannot obtain it by fetching `reverie/main`,
so the commands below cannot be run as written without a tree that already
carries that object. This is recorded here because the whole purpose of this
file is later re-examination, and the coordinates retained for this cell once
before could not be re-examined after their raw logs were gone.

- Measured Reverie `1393db6279331feb4af2533220e4786ab104a1b9` — pull-request
  head; verified 2026-08-31 as not an ancestor of `reverie/main`.
- Hermit main later moved the pin off it to main-line
  `ab07a89239150df3726a036bee9f5e897893dfc1`. The recovery commit records that
  the whole Reverie tree diff between the two is two SaBRe files and one DBT
  test script, with the DBT runtime source unchanged; see
  `tests/backend-parity/README.md`. `tests/DEBUGGING.md` records a validate run
  whose `pre.reverie_pin` node failed both attempts because `1393db62` was not
  reachable, which is the same fact from the other side.
- The Reverie pin has since moved again, to
  `af42d9cf7ae604777cd88c5cca5b319460c986e8`, which is Hermit main's pin as of
  2026-08-31 and IS an ancestor of `reverie/main`.

**This measurement is therefore evidence about its named tree and not about
current SaBRe.** https://github.com/rrnewton/reverie/pull/509, "Prevent
inherited SlotMap locks across process clones", merged 2026-08-27T12:50:44Z —
after this measurement was taken — adding 166 lines across
`experimental/reverie-sabre/` `thread.rs`, `ffi/clone.rs`, `slot_map.rs`,
`callbacks.rs` and `reverie_adapter.rs`. That is the same thread and clone area
as the pthread exit/join mechanism described above, and Hermit main's current
pin `af42d9cf` already contains it. Whether it moves this cell is unmeasured
here; do not read this file as saying either way.
