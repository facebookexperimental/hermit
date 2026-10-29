# gVisor syscall cases using Hermit's existing fixtures

This package builds five copied C++ syscall tests as ordinary first-party Buck
executables. Shared test utilities are compiled once as `:test_util`; each
executable uses the existing `hermit_bin_test` fixture. There is no vendor/import
system, custom shell runner, runtime-staging framework or shared lint/gate change.

Source: gVisor revision `d345f7da26f7c5c1b3d4c584b787fb1ac9c8cac7`.
The copied Apache notices are retained; these local copies are maintained here.
The original assertions and fixture setup remain intact:

- [`sched_yield.cc`](https://github.com/google/gvisor/blob/d345f7da26f7c5c1b3d4c584b787fb1ac9c8cac7/test/syscalls/linux/sched_yield.cc): `SchedYieldTest.Success`.
- [`clock_getres.cc`](https://github.com/google/gvisor/blob/d345f7da26f7c5c1b3d4c584b787fb1ac9c8cac7/test/syscalls/linux/clock_getres.cc): `ClockGetres.Timespec`.
- [`getrandom.cc`](https://github.com/google/gvisor/blob/d345f7da26f7c5c1b3d4c584b787fb1ac9c8cac7/test/syscalls/linux/getrandom.cc): `GetrandomTest.IsRandom`.
- [`eventfd.cc`](https://github.com/google/gvisor/blob/d345f7da26f7c5c1b3d4c584b787fb1ac9c8cac7/test/syscalls/linux/eventfd.cc): `EventfdTest.Nonblock`.
- [`poll.cc`](https://github.com/google/gvisor/blob/d345f7da26f7c5c1b3d4c584b787fb1ac9c8cac7/test/syscalls/linux/poll.cc): `PollTest.InvalidFds`.

## Setup and dependencies

`main.cc` selects exactly one compile-defined case per binary, calls the copied
`TestInit` / `RunAllTests`, and rejects zero, multiple or skipped executed cases.
Native ENOSYS can skip getrandom; that becomes nonzero exit 77, not passing
coverage. GoogleTest/GoogleMock assertions remain enabled in release builds.
Text timing/colour and benchmarks are disabled, native expectations selected,
save hooks off, and inherited XML/stream/result destinations cleared. An inherited
shard selection cached by GoogleTest before main can exclude the selected case;
that yields exit 78 from the zero-case check, never passing coverage.
The upstream SIGPIPE setup and Abseil/Benchmark initialisation remain.

Buck supplies GoogleTest, GoogleMock, Abseil, Benchmark and pthreads. The copied
errno-preserving new/delete implementations are direct binary inputs with
`allocator="malloc"`, avoiding an additional allocator. No selected case uses
runfiles: `test_util_runfiles.cc`, Bazel runfiles sources and non-Linux save
support are omitted. These guests use the same dynamic toolchain/runtime
assumptions as neighbouring Hermit first-party executables, with no extra staging.

Three local utility adaptations preserve syscall expectations:

- Unsized delete definitions explicitly declare `noexcept`, matching the C++
  runtime declarations.
- The poll timer lambda explicitly captures the `this` pointer, preserving its
  upstream pointer capture while avoiding deprecated C++20 implicit capture.
- `RetryEINTR` reuses arguments as lvalues on every attempt and deduces its return
  type from that call. It no longer repeatedly consumes owning rvalue arguments;
  syscall pointer/count/value callers retain their behaviour. A callable requiring
  an rvalue-only argument is not a reusable EINTR operation.

## Build and fixture coverage

```sh
buck2 build @fbcode//mode/dev-nosan fbcode//hermetic_infra/hermit/tests/gvisor: --show-full-json-output
buck2 test @fbcode//mode/dev-nosan fbcode//hermetic_infra/hermit/tests/gvisor: -- --timeout=180
```

Binary targets are `gvisor_<name>_test`; native target names are
`raw_run__custombin_gvisor_<name>_test`. Existing wrappers emit all eight legacy families:
raw, relaxed/default run, repeat verification, chaos, chaos replay, trace replay,
chaotic trace replay and record/verify. The existing host admission probe remains
in force, with no fallback to untraced execution. A missing host capability is
not a guest pass, and registration is not runtime qualification.

Raw execution is native conformance, not manifest `naked` variation testing.
Default run relaxes sequentialisation and deterministic I/O. Legacy verifier and
trace modes, including their existing schedule `--strip-times` option, are not
canonical L2 evidence. Modern manifest/backend cells, additional architectures
and sanitizer modes are not qualified by this pilot.
