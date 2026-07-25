# Record/replay status

This document records a Phase 2 readiness audit of Hermit's experimental
record/replay mode. The audit was run on 2026-07-22 against Hermit commit
`5d3b2a35870a` (`main` at the start of the audit).

## Summary

Hermit successfully recorded and replayed 13 of 16 audited workload rows. Both
processes exited with status zero and produced byte-identical stdout for every
passing row.

| Group | Passing | Audited | Result |
| --- | ---: | ---: | --- |
| Basic programs | 3 | 3 | 100% |
| Nondeterminism/language runtimes | 4 | 6 | 67% |
| OSS applications | 6 | 7 | 86% |
| **Total** | **13** | **16** | **81%** |

The strongest positive result is that record/replay handles the OpenMP
floating-point reduction, the unsynchronized pthread counter race, Python hash
randomization, and Go goroutine channel ordering. These workloads exercise
threads and nondeterministic inputs rather than only process startup.

The three failures are distinct:

- OpenJDK 8 does not finish recording even for `java -version`.
- Node.js aborts during recording when Reverie decodes `ioctl(FIOCLEX)`.
- SQLite records an in-memory query but replay consumes an unexpected `Mmap`
  event and aborts.

## Method

Every workload used a fresh recording directory. Record and replay were run as
separate commands, not through `record start --verify`, so their statuses and
outputs could be inspected independently:

```bash
timeout --kill-after=5s 90s \
  hermit record start --data-dir CASE/recordings -- PROGRAM ARGS...

timeout --kill-after=5s 90s \
  hermit replay --autopilot --data-dir CASE/recordings
```

A row passes only when record and replay both exit zero and replay stdout is
byte-identical to record stdout. The `--` before the program is required when
guest arguments begin with `-`; otherwise Clap interprets them as Hermit
options.

The host lacks CPUID faulting, so Reverie printed its normal CPUID warning. The
warning appeared in passing and failing cases and is not assigned as a failure
cause here.

## Environment

| Component | Version |
| --- | --- |
| Kernel | Linux `6.13.2-0_fbk13_hardened_0_g02230262e956`, x86_64 |
| Hermit | `5d3b2a35870a` |
| GCC | 11.5.0 |
| Go | `go1.26.4` (Red Hat 1.26.4-1.el9) |
| OpenJDK / javac | Temurin 1.8.0_492 |
| Node.js | 16.20.2 |
| `/usr/bin/python3` | 3.9.25 |
| Redis | 6.2.22 |
| SQLite | 3.34.1 |
| `/usr/bin/git` | 2.52.0 |
| curl | 7.76.1 |
| Ninja | 1.13.1, existing locally built binary |

## Basic programs

| Workload | Command | Record | Replay | Output | Status |
| --- | --- | --- | --- | --- | --- |
| C hello world | compiled `puts("hello world")` | 0 | 0 | identical | PASS |
| echo | `/bin/echo record-replay-echo` | 0 | 0 | identical | PASS |
| ls | `/bin/ls -1 /bin/echo /bin/ls` | 0 | 0 | identical | PASS |

`ls` is significant because it exercises directory/file metadata and dynamic
loader mappings beyond the minimal hello and echo cases.

## Nondeterminism and runtimes

| Workload | Program | Record | Replay | Output | Status |
| --- | --- | ---: | ---: | --- | --- |
| OpenMP FP reduction | Four threads, dynamic scheduling, float reduction | 0 | 0 | identical | PASS |
| pthread data race | Eight threads race on a volatile counter | 0 | 0 | identical | PASS |
| Python hash seed | `python3 -S -I hashseed_order.py` iterates a string set | 0 | 0 | identical | PASS |
| Go goroutines | 32 goroutines send IDs through a channel | 0 | 0 | identical | PASS |
| Java randomness | `SecureRandom.nextBytes(16)` | 124 | not run | none | FAIL: record timeout |
| Node randomness | `crypto.randomBytes(16)` | 1 | not run | none | FAIL: unsupported ioctl |

Fixture provenance:

- FP reduction: `impl-nondet-fp-reduction` branch,
  `tests/c/fp_reduction_nondeterminism.c`.
- pthread race: `impl-nondet-pthread-race` branch,
  `tests/c/pthread_race_nondeterminism.c`.
- Python: `impl-nondet-python-hashseed` branch,
  `tests/python/hashseed_order.py`.
- Go: `goroutine-channel-order` from the Go determinism experiment/PR.
- Java: a class that prints 16 bytes from `java.security.SecureRandom`.
- Node: a script that prints `crypto.randomBytes(16)` as hex.

Representative recorded outputs include:

```text
threads=4 bits=47000620 sum=0x1.000c4p+15
counter=2000000 expected=2000000 order=7,0,1,2,3,4,5,6
program=goroutine-channel-order ... order=31,30,...,00 sha256=ea892a...
```

### JVM failure

The default `SecureRandom` workload did not finish record startup within 90
seconds. A separate 30-second isolation probe showed the same behavior for
`java -version`. `-Xint`, `-XX:ActiveProcessorCount=1`, and `-XX:+UseSerialGC`
did not make the random workload complete. The only stderr was the host CPUID
warning; there was no recording ID or guest stdout.

This is a JVM-wide record-mode liveness problem, not a `SecureRandom`-specific
failure. The external timeout returned 124. Record mode needs an internal
startup/progress deadline and scheduler diagnostics before the exact blocked
thread or syscall can be identified efficiently.

### Node failure

Node fails at startup, including `node --version`, before the randomness
workload runs:

```text
reverie-syscalls/src/args/ioctl.rs:153
ioctl: unsupported request: FIOCLEX
```

The panic crosses a non-unwinding callback, aborts the container with SIGSEGV,
and returns exit 1. Supporting `FIOCLEX` (and making unknown ioctl decoding
fail safely) is the direct compatibility fix.

## OSS applications

| Workload | Command | Record | Replay | Output | Status |
| --- | --- | ---: | ---: | --- | --- |
| Redis identity | `redis-server --version` | 0 | 0 | identical | PASS |
| Redis memory test | `redis-server --test-memory 1` | 0 | 0 | identical | PASS |
| SQLite query | In-memory create/insert/sorted select | 0 | 1 | replay aborted | FAIL: event desync |
| Ninja identity | `ninja --version` | 0 | 0 | identical | PASS |
| Ninja dry run | One-edge build with `ninja -n` | 0 | 0 | identical | PASS |
| Git identity | `/usr/bin/git --version` | 0 | 0 | identical | PASS |
| curl identity | `/usr/bin/curl --version` | 0 | 0 | identical | PASS |

Redis's memory test is a real 1 MiB allocator/memory-integrity workload, not
only a version probe. It took approximately 19 seconds to record and 21 seconds
to replay. The Ninja dry run parses a build graph and plans one command without
changing the filesystem.

These probes do not establish full Redis server/client, network, Git checkout,
curl transfer, or Ninja build compatibility. Hermit does not snapshot a
changing filesystem or external network, so those workflows need controlled
inputs and explicit state reset between record and replay.

### SQLite failure

The query was:

```sql
CREATE TABLE t(x);
INSERT INTO t VALUES (3),(1),(2);
SELECT group_concat(x, ',') FROM (SELECT x FROM t ORDER BY x);
```

Record completed and printed `1,2,3`. Replay panicked in
`hermit-cli/src/replayer/macros.rs` because the next event was an unexpected
`SyscallEvent::Mmap`. The diagnostic serialized the entire mapping buffer,
producing approximately 650 KiB of stderr before the non-unwinding abort.

Recorder and replayer intentionally treat anonymous mappings as pass-through
and file mappings as captured `Mmap` events (`recorder/mmap.rs` and
`replayer/mmap.rs`). The failure means event consumption was already out of
alignment when replay reached this mapping; it should be reduced to the first
syscall/event mismatch rather than treated as only an address-placement issue.

## Existing regression suite

The repository's existing target also passes:

```text
cargo test -p hermit --test record_replay
17 passed; 0 failed; finished in 120.16s

cargo test -p hermit --test record_replay -- --test-threads=1
17 passed; 0 failed; finished in 95.90s
```

The target covers clock order, futex behavior, heap/stack pointers, nanosleep,
pipes, poll, RDTSC, scheduling, random data, and directory trees. Under default
parallelism, 14 tests exceeded 60 seconds before completing; serialization was
faster but still took about 96 seconds. This is useful coverage, but it is not a
substitute for runtime/OSS compatibility probes.

## Top Phase 2 blockers

1. **JVM record liveness.** OpenJDK cannot finish even `java -version` under
   record mode. Add an internal deadline plus scheduler/thread-state dumps,
   identify the blocked startup boundary, and add a Java startup regression.
2. **ioctl coverage and failure safety.** `FIOCLEX` prevents all tested Node
   startup. Add the request to Reverie's ioctl model and return a typed error or
   safe passthrough for unknown requests instead of panicking across FFI.
3. **mmap/event-stream alignment.** SQLite exposes an unexpected captured
   `Mmap` event. Build a minimal mmap-heavy reproducer, report the preceding
   syscall/event counts, and fix FD/mapping event consumption before attempting
   file-backed database replay.
4. **Failure containment and diagnostics.** Replay mismatches currently panic;
   Node becomes SIGSEGV, SQLite dumps hundreds of KiB, and JVM supplies no
   progress information. Time-travel debugging needs bounded, structured errors
   with recording ID, thread, event index, expected type, and actual type.
5. **Stateful I/O coverage.** Recorder TODOs still call out FD tracking and
   missing `ppoll`, `epoll`, and `select` support. Full Redis, build, and network
   workflows also need immutable filesystem/network inputs or an explicit
   snapshot/reset contract. Add these as a ratcheted compatibility matrix rather
   than extrapolating from version probes.

## Recommended next checks

After the top three compatibility fixes, rerun this matrix and add:

- a Redis server plus `redis-cli` transaction on isolated loopback;
- SQLite against a controlled on-disk database as well as `:memory:`;
- an actual Ninja build with the output tree reset before replay;
- Node worker threads and Java thread/futex workloads;
- repeated replay of one retained recording, including GDB attach and stepping.

Keep the Hermit revision, executable, arguments, environment, filesystem, and
recording directory unchanged between record and replay. Use isolated data
directories so a failed case cannot select another case's `last` recording.
