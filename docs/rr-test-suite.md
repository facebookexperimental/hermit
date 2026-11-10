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

Validation runs every case that is not `#[ignore]`d, in the pinned root:
`test.rr_suite_contract` runs 192 rr programs plus the scratch-directory
contract test, eight at a time (233 s of wall time when it ran serially,
measured 2026-10-15). Its GitHub
hosted twin leaves out `rr_no_mask_timeslice` by exact name. That program
needs the PMU preemption timer, and hosted runners have no PMU. The programs
are ptrace-heavy and need PMU branch counters plus working user and mount
namespaces, so run them on such a host:

```sh
cargo test -p hermit-run --test rr_suite              # every enabled case
cargo test -p hermit-run --test rr_suite rr_hello     # one
cargo test -p hermit-run --test rr_suite -- --ignored # only the known failures
```

## Coverage

Starting from the fbsource `RR_TEST_TARGETS` minus the tests fbsource already
disables under Hermit (its `wrap_test_suite(exclude=[...])` list), **217** rr
programs build against this checkout and have an `rr_test!` case. **192** run in
validation; the 2026-10-17 test audit deleted one case, listed under
"Programs with no case". Measured 2026-10-15 at main ec49cdd7, 25 of the then 217 had stopped
passing while every case was `#[ignore]`d and no validation node ran them.
Each that still cannot pass carries an `#[ignore = "..."]` naming its reason
(https://github.com/rrnewton/hermit/issues/4257):

- 19 are refusals by design that an E2E cell or Detcore policy already covers:
  - `keyctl`;
  - 8 `seccomp_*` cases that need `prctl(PR_SET_NO_NEW_PRIVS)`, and
    `seccomp_desched`;
  - `shm` and `shm_unmap`;
  - `splice` and `vm_readv_writev`;
  - `prctl_speculation_ctrl`, `rlimit`, `quotactl`, `setgroups` and `setuid`.
- 5 are gaps (`multiple_pending_signals` left this list when Detcore began
  choosing a process-directed signal's receiving thread,
  https://github.com/rrnewton/hermit/issues/4034):
  - `fatal_init_signal` and `unexpected_exit_pid_ns`;
  - `numa`;
  - `sched_yield_to_lower_priority`;
  - `intr_ppoll` (refused with exit 122).
- `spinlock_priorities` passes, in 43 s on a developer host, but spins past
  validation's 22 CPU-second per-test budget.

Each run is wrapped in `timeout(1)` (`120s`) with a `10s` TERM-to-KILL grace
period, so any future hang fails that test cleanly rather than blocking the
serialized suite. Special cases carried over from fbsource:

- `rr_args` runs with `-no --force-syscall-buffer=foo -c 1000 hello` (exit 0).
- `rr_pause` expects exit code 1 and must print its final `EXIT-SUCCESS` marker.

## Programs with no case

These upstream rr programs build but have no `rr_test!` case
(`rr_multiple_pending_signals_sequential` left this list with
https://github.com/rrnewton/hermit/issues/4034; it now has a case):

| rr test | symptom | likely Hermit gap |
| --- | --- | --- |
| `rr_simple_threads_stress` | deleted by the same audit: `join_threads`'s 100-thread create/join loop without its `test_assert`s | none; `rr_join_threads` runs the same loop and asserts each call |

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
