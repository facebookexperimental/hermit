# Record/replay compatibility matrix: 2026-07-21

This experiment exercises Hermit's user-facing `record start` and `replay
--autopilot` commands against common utilities, an interpreter, a compiler,
and two pthread workloads. It records the exit status and guest stdout/stderr
from both phases, retains Hermit's per-thread event streams, and checks whether
the replay observation is byte-for-byte identical to the recording.

## Result

Six of eleven workloads recorded successfully, replayed successfully, and
produced identical output and exit status on the tested speculative build.

| Program | Record | Replay | Output | Exit | Observation |
| --- | --- | --- | --- | --- | --- |
| `echo` | pass | pass | match | match | One recorded event stream. |
| `ls` | pass | pass | match | match | Stable listing of the fixture tree. |
| `cat` | pass | pass | match | match | Stable file contents. |
| `grep` | pass | pass | match | match | Stable matching lines. |
| `find` | pass | fail | mismatch | mismatch | Replay panics in `hermit-cli/src/replayer/fs.rs:67` on `Got unexpected event: Return(0)`. |
| `sort` | pass | pass | match | match | Largest successful trace at 15,455,895 bytes. |
| `wc` | pass | pass | match | match | Stable count and fixture path. |
| `python3 -c` | fail | not run | unavailable | unavailable | Recording timed out after 60 seconds without producing a recording. |
| `gcc -c` | fail | not run | unavailable | unavailable | Recording timed out after 60 seconds without producing a recording. |
| `pthread_create` | fail | fail | match | match | Record and replay both abort with status 134 and `The futex facility returned an unexpected error code.`; five event streams were captured. |
| producer-consumer | fail | fail | match | match | The same reproducible futex abort occurs with three event streams. |

`record_success` and `replay_success` require zero exit status. The separate
output and exit columns remain meaningful for nonzero runs: the two pthread
cases demonstrate that Hermit can reproduce the observed failure even though
it cannot run either workload successfully.

The complete machine-readable observations, hashes, commands, event-stream
counts, and event byte totals are in [`results.tsv`](results.tsv). Host and
binary metadata are in [`metadata.txt`](metadata.txt).

## Reproduce

From the repository root:

```sh
cargo build -p hermit --bin hermit
./experiments/record-replay-matrix_20260721/run_matrix.sh
```

The runner resolves the requested system programs from `PATH`, compiles the
two C pthread fixtures under `target/record-replay-matrix/`, and gives each
record and replay phase 60 seconds. Override the binary or timeout with
`HERMIT_BIN` and `CASE_TIMEOUT_SECONDS`.

For every workload the runner preserves:

- recording metadata and per-thread event/debug streams;
- raw and guest-normalized record/replay stdout and stderr;
- record/replay process statuses and the exact shell-escaped command.

Raw artifacts are written below the ignored `artifacts/` directory. The final
run occupied 140 MB, so traces and copied executables are intentionally not
committed. The TSV retains their event-stream counts, aggregate sizes, and
output hashes. Each run uses a new UTC timestamped artifact directory and
refuses to overwrite an existing one.

## Method

The reported run used Hermit commit
`96261f618dda654fb87ffacd0e178c4bf743faaf` from the fork's `speculative`
line on Linux 6.13.2, x86-64, with an AMD EPYC 9D85 CPU. Routine Hermit logging
was disabled. The record-completion banner and GNU `timeout` diagnostics are
removed before comparing guest stderr; the unmodified raw streams remain in
the artifact directory.

Filesystem fixtures are committed beside the runner so `ls`, `cat`, `grep`,
`find`, `sort`, and `wc` receive stable inputs. The pthread fixtures avoid
nondeterministic output: one joins four independent workers, and the other is
a mutex/condition-variable bounded producer-consumer queue.

This is a one-run compatibility matrix, not a statistical reliability claim.
It establishes concrete support and failure modes for these commands on this
host and build.
