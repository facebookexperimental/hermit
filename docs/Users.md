# Hermit Users Guide

**Hermit is a a program sandbox for repeatability & concurrency testing.**

Hermit, by the Hermetic Infra Team, launches programs in a special sandbox to control their execution. Hermit translates normal, nondeterministic Linux behavior, into deterministic, repeatable behavior.  This can be used for various applications, including:

- record-replay debugging,
- simple reproducibility,
- "chaos mode" to expose concurrency bugs in a controlled and repeatable way.

Hermit is a middleware (or “sentry”) that sits between the guest process and the OS, accepting guest system call requests on one side, and producing sending system calls to the real Linux kernel on the other side.

Hermit currently supports x86_64 Linux and can be run via:

```
~/fbsource/fbcode/hermetic_infra/hermit/hermit
```

And here's a minimal demo of running inside hermit's deterministic environment:
```
$ cd ~/fbsource/fbcode/hermetic_infra/hermit/
$ ./examples/race.sh
bbbbbbaa (... nondeterministic output...)
$ ./hermit run --strict --epoch=2026-01-01T00:00:00Z ./examples/race.sh
abababab (... deterministic racing processes...)
```

Deterministic execution is the default: `./hermit run ./examples/race.sh`
runs the same way, taking its virtual-time epoch from the host clock and
reporting it so the run can be repeated. `--strict` also requires every input
of the run on the command line, so it needs `--epoch` (or `HERMIT_EPOCH`). See
[USER_GUIDE.md](USER_GUIDE.md) for the full set of mode-selection options.

To debug a run, add `--gdbserver` and attach gdb with
`gdb -ex 'target remote 127.0.0.1:1234' PROGRAM`. This works for a `--strict`
run too: its guest keeps the isolated network, and every strict refusal
applies. What the debugger does is an input of the run; see
[USER_GUIDE.md](USER_GUIDE.md) for what that changes.

Hermit preserves standard input, output and error at CLI entry and closes other
unsolicited file descriptors inherited from its caller. Pass guest inputs through
the documented file, mount and standard-stream options. An operating system that
cannot perform this descriptor cleanup is refused before a guest starts.

Each `hermit run` gets its own empty `/tmp`, `/dev/shm` and IPC namespace (a
recording, its own `/dev/shm` and IPC namespace), so files, POSIX shared
memory objects and message queues do not pass between concurrent runs, or
between a run and the host, unless you bind them in: `--bind` for files and
shared memory objects, `--mount type=bind,source=/dev/mqueue,target=/dev/mqueue`
for message queues (see [USER_GUIDE.md](USER_GUIDE.md)).

## Further reading

* Find hermit [CLI examples here](https://fb.workplace.com/notes/hermetic-infra-fyi/hermit-tech-preview-a-linux-reproducibility-tool/244656753248444/)
* See demos in [this tech talk](https://fb.workplace.com/groups/591973351138875/permalink/1132872253715646/).
* [This talk](https://fb.workplace.com/groups/591973351138875/posts/1533285603674307) shows concurrency testing with hermit "chaos" mode.
* [This talk](https://fb.workplace.com/groups/hermit.fyi/post
