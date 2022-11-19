# Hermit: A reproducible container

Hermit forces deterministic execution of arbitrary programs and acts like a
reproducible container. That is, it *hermetically* isolates the program from
sources of non-determinism such as time, thread interleavings, random number
generation, etc. Guaranteed determinism is a powerful tool and it serves as a
basis for a number of applications, including concurrency stress testing,
record/replay, reproducible builds, and automatic diagnosis of concurrency bugs,
and more.

Hermit cannot isolate the guest program from sources of non-determinism such as
file system changes or external network responses. Instead, in order to provide
complete determinism, the user should provide a fixed file system base image
(e.g., with Docker) and disable external networking.

# How it works

Hermit sits between the guest process and the OS intercepting system calls made
by the guest (using [Reverie][]). In some cases, it can completely replace the
functionality of the kernel and suppress the original system call. In other
cases, it forwards the system call to the kernel and sanitizes the response such
that it is made deterministic.

As a concrete example, lets say we have a program that reads random bytes from
`/dev/urandom`. Hermit will see that the guest opened this file (a known source
of non-determinism) and intercept subsequent reads to this file. Instead of
letting the OS fill a buffer with random bytes, Hermit uses a deterministic
pseudorandom number generator with a fixed seed to fill in the buffer. The
contents of the buffer are then guaranteed to be the same upon every execution
of the program.

The most complex source of non-determinism is in the thread scheduler. The way
threads are scheduled by the kernel depends on many external factors, including
the number of physical CPUs or other threads running on the system that require
CPU time. To ensure that the threads of the guest process (and all of its child
processes) are scheduled in a repeatable way, we first make sure that all thread
executions are serialized so that there is effectively only one CPU. Then, we
deterministically pick which thread is allowed to run next. In order to only
allow a thread to run for a fixed number of instructions, we use the CPU's
Performance Monitoring Unit (PMU) to stop execution after a fixed number of
retired conditional branches (RCBs).

Read below about how to build Hermit, and you can get an idea of what it does
from running examples in the [./examples](./examples) folder.

[Reverie]: https://github.com/facebookexperimental/reverie

# Building and running Hermit

Hermit is built with the standard Rust cargo tool.

```bash
cargo build
```
This builds the whole cargo workspace. The actual binary is located in target
directory (`target/debug/hermit`).

Then, once you've built Hermit, all you need to run your program
deterministically is:

```bash
hermit run <prog>
```

After that you can try running it in a concurrency stress testing (chaos) mode,
or varying other parameters of the configuration such as the speed at which
virtual time passes inside the container, or the random number generation seed:

```bash
hermit run --chaos --seed=3 <prog>
```

You can use hermit as a replay-debugger as well, either recording a
non-deterministic execution (real time, real randomness, etc), or repeatedly
running a controlled, deterministic one (virtual time, pseudo-randomness, etc).

```bash
hermit record <prog>
hermit replay
```

# Example programs

See the [the examples folder](./examples/README.md) for example programs and
instructions on how to run them.  These showcase different sources of
nondeterminism, and how hermit eliminates or controls them.

In order to explore more advanced examples, you can look at some of
the integration tests built from [./tests/`]() or [./flaky-tests/]().
For example, using the commands below you can run a racy example
multiple times to see its nondeterminism.  Then run it under hermit to
watch that determinism disappear.  Then run it under hermit `--chaos`
to bring that nondeterminism back, but in a controlled way that can be
reproduced based on the input seed.

```bash
cargo build
for ((i=0; i<20; i++)); do ./target/debug/hello_race; done

for ((i=0; i<20; i++)); do hermit run ./target/debug/hello_race; done

for ((i=0; i<20; i++)); do
  hermit run --chaos --seed-from=SystemRandom ./target/debug/hello_race;
done
```

# The state of CI and testing

At Meta, this repository is built using buck.  We have over 700 integration
tests that run under this setup. But as of this initial release (2022-11-21), we
have not ported these tests to an external build system yet.

A few unit tests run under `cargo test`, but the integration tests are more
complicated because they combine various run modes with each of the test
binaries (which are built from `tests/`, `flaky-tests/`, and the rr test suite
too).

We plan to get the internal Buck configuration files building externally with
buck or bazel.

# Applications

Hermit translates normal, non-deterministic Linux behavior, into deterministic,
repeatable behavior. This can be used for various applications, including:
record/replay debugging, simple reproducibility, "chaos mode" to expose
concurrency bugs in a controlled and repeatable way. Generally, Hermit makes
implicit inputs into explicit ones, and so enables searching over possible
executions by explicitly varying these inputs and study the changes in outcomes.
This can be used for either searching for bugs or trying to narrow down their
causes.

## Diagnosing concurrency bugs

One experimental application that Hermit has built-in support for is diagnosing
concurrency bugs, in terms of identifying the stack traces of racing critical
operations which, if their order is flipped in the schedule, cause the program
to crash (also called an "order violation" bug). Hermit can detect these even if
the racing instructions are in different processes and written in different
programming languages.

You can kick off analyze with any program invocation, and tell it to search for
failing (and passing) executions, and then diagnose the difference between them.

```
hermit analyze --search -- <run_args> <prog>
```

# License

Hermit is licensed under a BSD-3 clause license, included in the `LICENSE` file
in this directory.

# Support

Hermit currently supports x86_64 Linux. Aarch64 support is a
work in progress.
