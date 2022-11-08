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
$ ./hermit run --strict ./examples/race.sh
abababab (... deterministic racing processes...)
```

## Further reading

* Find hermit [CLI examples here](https://fb.workplace.com/notes/hermetic-infra-fyi/hermit-tech-preview-a-linux-reproducibility-tool/244656753248444/)
* See demos in [this tech talk](https://fb.workplace.com/groups/591973351138875/permalink/1132872253715646/).
* [This talk](https://fb.workplace.com/groups/591973351138875/posts/1533285603674307) shows concurrency testing with hermit "chaos" mode.
* [This talk](https://fb.workplace.com/groups/hermit.fyi/post
