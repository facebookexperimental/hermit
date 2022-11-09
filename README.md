Hermit: a reproducible Linux container
======================================

This directory contains a Rust project that builds the "hermit" binary.  Hermit can sandbox the execution of a guest program (and its children).  It isolates the guest from the host platform and any nondeterministic responses by the underlying Linux kernel. It is a container in certain respects (process namespace, network namespace), but it doesn't hide the host file system from the guest.  Rather, hermit is userspace software that is typically run inside another container (such as Docker) which provides the file system base image. Like Wine or WSL(1), Hermit sits between the guest process and the OS, accepting guest system call requests on one side, and producing sending system calls to the Linux kernel on the other side.

Read below about how to build hermit, and you can get an idea of what it does from running examples in the [./examples](./examples) folder.

Building hermit
===============

Hermit is built with the standard Rust cargo tool.

```bash
cargo build hermit
```

Then, once you've built hermit, that's all you need to run your program deterministically inside the sandbox.

```bash
hermit run <prog>
```

After that you can try running it in a concurrency stress testing (chaos) mode, or varying other parameters of the configuration such as the speed virtual time passes inside the container, or the random number generation seed.

```bash
hermit run --chaos --seed=3 <prog>
```

You can use hermit as a replay-debugger as well, either recording a nondeterministic execution (real time, real randomness, etc), or repeatedly running a controlled, deterministic one (virtual time, pseudo-randomness, etc).

```
hermit record <prog>
hermit replay
```

Applications
============

Hermit translates normal, nondeterministic Linux behavior, into deterministic, repeatable behavior. This can be used for, various applications, including: record-replay debugging, simple reproducibility,"chaos mode" to expose concurrency bugs in a controlled and repeatable way.  More specifically:

Diagnosing concurrency bugs
---------------------------

One application that hermit has built-in support for is diagnosing concurrency bugs, in terms of identify the stack traces of racing critical operations which, if their order is flipped in the schedule, cause the program to crash (also called an "order violation" bug).  Hermit can detect these even if the racing instructions are in different processes and written in different programming languages.

You can kick off analyze with any program invocation, and tell it to search for failing (and passing) executions, and then diagnose the difference between them.

```
hermit analyze --search -- <run_args> <prog>
```

License
=======

Hermit is licensed under a BSD-3 clause license, included in the `LICENSE` file in this directory.

Support
=======

Hermit currently supports x86_64 Linux and can be run via.  Arm support is a work in progress.

