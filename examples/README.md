# Hermit - Dynamic determinism enforcement and record/replay

## Overview

Hermit uses a combination of lightweight containerization and system call
interception to identify and compensate for potentially random behavior that may
occur during the execution of a given program or command. In this way, hermit
can be used to launch arbitrary programs and run them in a way that is
deterministic and reproducible.

This initial release supports a substantial number of Linux features. But it's a
large surface area, and compatibility is not 100%. If you find that your program
errors or hangs under hermit, report a bug here:

   https://github.com/facebookexperimental/hermit/issues

## Basic Usage

Typical usage of the hermit tool consists of simply placing `hermit run` at the
beginning of the command that is to be executed. For example, running the script
`my-example.sh` would be achieved by the following command:

```shell
hermit run ./my-example.sh
```

Similarly, we could run `hermit run -- ls -l`, revealing that the process run
under hermit can still by default access the full file system, including
`/bin/ls`. More detailed control over the starting conditions of the
deterministic computation is enabled with additional command line flags. To see
these flags and how to use them, please refer to the command line help info:

```shell
hermit --help
hermit run --help
```

## Examples

To run the example scripts in this directory, first switch to it.  Next, run one
of the examples a few times without hermit. For example, running `./rand.py`
should output a new series of random numbers on each execution:

```shell
$ ./rand.py
57 55 68 49 11 68 88 43 2 97
$ ./rand.py
17 12 63 1 92 76 75 68 13 81
$ ./rand.py
98 44 97 98 35 40 40 10 36 19
```

Finally, run the same script a few more times, but this time, use `hermit` to
enforce determinism:

```shell
$ hermit run ./rand.py
55 8 80 78 8 35 14 60 71 78
$ hermit run ./rand.py
55 8 80 78 8 35 14 60 71 78
$ hermit run ./rand.py
55 8 80 78 8 35 14 60 71 78
```

Notice the key difference here: every execution returns the same results!

In addition to `rand.py`, you can also try the following example scripts:

- `race.sh`: Creates two simultaneously-running processes, one printing `a`
and the other printing `b`. Normally, this generates an output string of
randomly interleaved `a`s and `b`s, but calling this script with `hermit run`
results in a consistent, reproducible output string.

- `date.sh`: Retrieves the current date and time via the `date` command.
Normally, the output of this command would change on subsequent calls as
time elapses. However, running with `hermit run` ensures that the same
date output is received on every call.

- `devrand.sh`: Generates random numbers similar to `rand.py`, but uses
`/dev/random` as the source of its data. **Note:** Reading from `/dev/random`
will block until sufficient entropy is available to generate the random output,
so this example can take a while to run.

It is highly reccommended to take a look at the contents of these example
scripts to see what is being tested.

It is also worthwhile to experiment with running arbitrary commands in
`hermit`. One such experiment could be reading directly from `/dev/urandom`:

```shell
$ head -c 16 /dev/urandom | hexdump
0000000 9808 6ead 7593 4497 6435 a7d9 876d 8720
0000010
$ head -c 16 /dev/urandom | hexdump
0000000 31b6 8790 f480 8c05 62c5 653f fdbd 27ba
0000010
$ head -c 16 /dev/urandom | hexdump
0000000 e8aa f3e1 95be 0780 4cce 2a15 edf9 d6c9
0000010
```

Again, we see random data being generated. Next, run the same command with
`hermit`:
```shell
$ hermit run -- head -c 16 /dev/urandom | hexdump
0000000 3211 d873 5fc1 a37b d83b cf8d ea15 69c2
0000010
$ hermit run -- head -c 16 /dev/urandom | hexdump
0000000 3211 d873 5fc1 a37b d83b cf8d ea15 69c2
0000010
$ hermit run -- head -c 16 /dev/urandom | hexdump
0000000 3211 d873 5fc1 a37b d83b cf8d ea15 69c2
0000010
```

As expected, the command is now running deterministically and generates
the same output on each execution.
