# rr syscall test suite under Hermit

Hermit reuses the focused syscall edge-case test programs from the
[rr](https://github.com/rr-debugger/rr) record/replay debugger. Each rr test is a
small standalone C program that exercises one syscall or kernel corner case and
`assert()`s the observed behavior. Running them under `hermit run` is a strong
regression guardrail: Hermit must reproduce real kernel semantics deterministically.

This mirrors the fbsource `RR_TEST_TARGETS` set defined in
`hermetic_infra/common/wrap_test_suite.bzl`.

## Layout

- **Sources:** the pinned `third-party/rr` git submodule (upstream commit
  `39e5c18`). Initialize it with:
  ```sh
  git submodule update --init third-party/rr
  ```
- **Harness:** `hermit-cli/tests/rr_suite.rs`. It generates rr's syscall-enum
  headers with rr's own `generate_syscalls.py`, freshly compiles each
  `src/test/<name>.c` invocation with rr's `RR_TEST_FLAGS`
  (`-D_FILE_OFFSET_BITS=64 -pthread -std=gnu11 -g3 -O0`, linked against
  `-ldl -lrt`), and runs it as:
  ```sh
  hermit run --base-env=minimal --max-timeslice=80000000 -- <program> [args]
  ```
  asserting the expected exit code. Each invocation uses a unique temporary
  working directory that is removed after the test.

## Running

The programs are ptrace-heavy and depend on PMU branch counters plus working
user/mount namespaces, so they are `#[ignore]`d by default (like the other Hermit
integration suites) and run explicitly:

```sh
cargo test -p hermit --test rr_suite -- --ignored          # all
cargo test -p hermit --test rr_suite -- --ignored rr_hello # one
```

`scripts/validate.rs` runs the full suite as its "rr syscall suite" check.

## Coverage

Starting from the fbsource `RR_TEST_TARGETS` minus the tests fbsource already
disables under Hermit (its `wrap_test_suite(exclude=[...])` list), **218** rr
programs build against this checkout, and the harness enables the **217** that
pass when each program receives a fresh scratch directory.
`rr_rusage`, `rr_sigchld_interrupt_signal`,
`rr_sigprocmask_in_syscallbuf_sighandler` and `rr_spinlock_priorities` were
enabled on 2026-10-13, after they passed 3 of 3 runs each (`rr_rusage` 2 of 2)
at main `d851dee4`. Each run is
wrapped in `timeout(1)` (`120s`) with a `10s` TERM-to-KILL grace period so any
future hang fails that test cleanly rather than blocking the serialized suite.
Special cases carried over from fbsource:

- `rr_args` runs with `-no --force-syscall-buffer=foo -c 1000 hello` (exit 0).
- `rr_pause` expects exit code 1 and must print its final `EXIT-SUCCESS` marker.

## Known failures (not yet enabled) — bugs to file

These upstream rr programs build but do **not** pass under `hermit run` today.
They are excluded from the harness and tracked here:

| rr test | symptom | likely Hermit gap |
| --- | --- | --- |
| `rr_multiple_pending_signals_sequential` | refused by name (exit 122): "process-directed signal 10 to process 3 by kill is not supported: the sending thread blocks the signal"; it used to hang intermittently | https://github.com/rrnewton/hermit/issues/4034, tracked for this test in https://github.com/rrnewton/hermit/issues/116 |

`rr_arch_prctl` (fbsource `test_arch_prctl`) has no plain `src/test/arch_prctl.c`
upstream — only `src/test/x86/arch_prctl_x86.c` and `arch_prctl_xstate.c` — so it
is not mapped here.

Beyond these, fbsource itself disables ~78 further rr tests under Hermit (known
nondeterministic, timing-sensitive, or requiring a special test runner); those
remain out of scope and are enumerated by the `exclude` list in the fbsource
`tests/BUCK` `wrap_test_suite(...)` call.

## Refreshing

To advance the pinned rr version, update the submodule and re-triage which
programs pass, then adjust the `rr_test!` list in `hermit-cli/tests/rr_suite.rs`.
