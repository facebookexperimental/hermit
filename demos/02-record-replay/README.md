# Demo 2: Record and replay

Hermit can record a program's execution to disk and replay it later, feeding the
program the same system-call results it saw the first time. A replay can run
unattended to completion, or it can be driven from GDB. This demo records
`/bin/echo`, lists the recording, replays it, records and checks a replay in one
step, and finally replays the recording under a scripted GDB session.

Record/replay is experimental and narrower than `hermit run`: programs that run
deterministically under `hermit run` do not all record and replay. This demo
uses a single-threaded program that makes few system calls.

## Prerequisites

- `hermit` on your `PATH`, built from this checkout (see
  [Setup](../README.md#setup)).
- `cargo` and a C compiler (the shared demo setup builds two small test
  programs).
- `gdb` for the last step, and `timeout` from coreutils.

## Run it

From the repository root:

```bash
demos/02-record-replay/run.sh
```

The same demo runs with `make -C demos demo2`. It takes about 6 to 7 seconds
once the shared test programs are built.

To try it yourself:

```bash
mkdir -p /tmp/hermit-recordings
hermit --log=error record start --data-dir=/tmp/hermit-recordings -- /bin/echo recorded
hermit record list --data-dir=/tmp/hermit-recordings
hermit --log=error replay --autopilot --data-dir=/tmp/hermit-recordings
hermit --log=info record start --verify --data-dir=/tmp/hermit-recordings -- /bin/echo verified-recording
```

Keep the recording directory, the program, its inputs, and the Hermit build the
same between recording and replay.

`record start` ends with a hint such as `hermit replay 549d235e49244e35b1ea683262fcbbcb`.
That hint leaves out `--data-dir`, so pasted as printed it looks in the default
directory (`~/.cache/hermit`) and fails with `Failed to open
".../metadata.json"`. Add the same `--data-dir` you recorded with:

```bash
hermit --log=error replay --autopilot --data-dir=/tmp/hermit-recordings <recording id>
```

Without an id, `hermit replay` replays the most recent recording in the data
directory. The `record start --verify` step does not change which one that is:
its recording is temporary and is deleted after the check.

For an interactive debugging session, leave out `--autopilot`:

```bash
hermit replay --data-dir=/tmp/hermit-recordings
```

This starts a replay GDB server on port 1234 and a GDB client connected to it.
The client stops at the program's first instruction, in the dynamic loader's
`_start`, and waits for your commands (`continue`, `break`, `stepi`, and so on).
Type `continue` and the replayed program runs to the end: it prints `recorded`,
and GDB reports `[Inferior 1 (process 3) exited normally]`. Type `quit` to
leave GDB.

## What you will see

Output of `demos/02-record-replay/run.sh` on 2026-09-30, with Hermit built from
commit `dc92644f96f4`. The recording id is generated per recording. Most of
GDB's startup and symbol-loading messages are left out (shown as `...`); they
depend on your GDB version and on where your system keeps debug symbols.

```text
==========================================
===     Demo 2: Record And Replay      ===
==========================================

Hermit records an execution into an isolated data directory, lists the recording
in text and JSON, and replays it to completion with --autopilot. It can also
record and immediately verify a replay. Without --autopilot, hermit replay
starts a replay gdbserver and GDB client; the demo drives a noninteractive GDB
session that continues the guest to completion. Keep the recording directory,
executable, inputs, and Hermit revision unchanged between recording and replay.

==========================================
Using hermit 0.2.0 (2026-09-30, gdc92644f96f4-dirty) (...)

=== Record /bin/echo, list the recording, and replay it ===
recorded

RECORDING COMPLETE! To replay, run:

    hermit replay d84b04b89c7c438ab349307e23aa8b11

d84b04b89c7c438ab349307e23aa8b11  /bin/echo recorded
[{"id":"d84b04b89c7c438ab349307e23aa8b11","program":"/bin/echo","args":["recorded"]}]
recorded

=== Record and immediately verify a replay (temp recording auto-deleted) ===
:: Recording...
:: Replaying...
:: Comparing captured verification logs...
Logs contain 276 | 276 messages total
Logs contain 275 | 275 detcore-specific messages
Logs contain 276 | 276 INFO messages
Logs contain 266 | 266 DETLOG & scheduler COMMIT messages
Normalizing known nondeterministic numerical data before comparison...
  Comparing DETLOG messages...

Done processing logs, no substantive differences found (266 | 266 DETLOG messages compared).
Logs contain 1 | 1 scheduler empty-run-queue kick messages
Logs contain 0 | 0 scheduler COMMIT records reading /proc/self/maps
:: comparison=Stripped relaxations=unsafe-numeric-address-and-path-normalization/v1
:: Success: replay matched recording.

=== Replay under GDB (noninteractive: continue to completion) ===
...
Reading symbols from /tmp/hermit-demo.2NiSHF/recordings/d84b04b89c7c438ab349307e23aa8b11/exe...
...
Remote debugging using :1234
...
0x00007ffff7fe3010 in _start () from target:/lib64/ld-linux-x86-64.so.2
Continuing.
...
recorded
[Inferior 1 (process 3) exited normally]

=== Demo 2: Record And Replay: SUCCESS ===
```

The `Using` line shows your build's date, commit, and path (the path is left
out here). Running the demo again gives the same output apart from the
recording id and the temporary directory name.

## What to notice

- `hermit record list` shows each recording with the program and arguments it
  ran; `--json` gives the same list in a machine-readable form.
- `replay --autopilot` prints `recorded` again: the replay reruns the program
  and feeds it the recorded system-call results.
- `record start --verify` records, replays at once, and compares Hermit's logs
  from the two executions. It prints the comparison but not the program's own
  output, so `verified-recording` does not appear. It needs `--log=info`,
  because at `--log=error` the log it compares is empty.
- The GDB step passes `--gdbex=continue` and then `--gdbex=quit`. Without the
  final `quit`, GDB would wait at its prompt after the program exits. GDB sees
  the replayed program as process 3, the process ID Hermit's container gives
  it, not the host's.

## How it works

While recording, Hermit writes the results of the program's nondeterministic
system calls, together with its scheduling decisions, into a data directory.
During replay it runs the same program and answers those calls from the
recording instead of from the kernel, so the program follows the same path.
Without `--autopilot`, Hermit exposes the replayed program through a GDB server
so you can set breakpoints and step through an execution that is guaranteed to
behave the same way every time.
