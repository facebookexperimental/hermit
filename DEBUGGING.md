# Debugging Programs Under Hermit

Hermit makes a program's execution a deterministic function of its inputs.
For debugging, that means two things. Given the same inputs, a bug that shows
up in a `hermit run` shows up again, at the same place, when you rerun it.
The inputs include the virtual clock's starting time, so pass a fixed
`--epoch` (otherwise each run starts from the host's current time), the files
the program reads, and anything it receives over a network, which Hermit
does not make deterministic. And a `hermit record` recording, which captures
the program's inputs, can be replayed as often as you like, so a debugger can
move backwards through it: the debug adapter restarts the deterministic
replay and runs it forward to the earlier point.

This page covers the debugger interfaces Hermit has today and how to drive
them from GDB, from a Debug Adapter Protocol (DAP) client, and from an agent.
Editor (VS Code) integration is a prototype; [its section](#vs-code) says
exactly what works and what does not yet.

| Goal | Command | Reverse execution |
| --- | --- | --- |
| GDB on a deterministic run | `hermit run --gdbserver -- PROGRAM`, then `gdb PROGRAM -ex 'target remote :1234'` | No |
| GDB on a recording | `hermit replay [ID]` (starts GDB itself) | No |
| DAP client on a deterministic run | `hermit run --gdbserver -- PROGRAM`, then a DAP client running `hermit-dap` | No |
| DAP client on a recording, over TCP | `hermit replay [ID] --dap-listen=4711` | `stepBack`, `reverseContinue` |
| DAP client on a recording, over stdio | a DAP client running `hermit-dap --replay ID` | `stepBack`, `reverseContinue` |

Everything here is experimental and has been tested on single-threaded C
programs only. Multithreaded guests are not yet covered by the tests.

## Requirements

- Build `hermit` and `hermit-dap` together. `hermit replay --dap-listen` runs
  the `hermit-dap` beside the `hermit` executable, and `hermit-dap --replay`
  runs the `hermit` beside itself:

  ```bash
  cargo build -p hermit --bin hermit --bin hermit-dap
  ```

  This rebuilds `target/debug/hermit` with default features. If you built it
  with extra backends (see the README), repeat that build afterwards. The
  examples below run `hermit` and `hermit-dap` from `PATH`; to use the ones
  you just built rather than an installed copy, put them first:

  ```bash
  export PATH="$PWD/target/debug:$PATH"
  ```
- For DAP, a GDB with the DAP interpreter. The adapter is tested end to end
  with GDB 17.2 only. `hermit-dap` uses `/usr/bin/gdb` when that file
  exists, and otherwise `gdb` on `PATH`; pass `--gdb PATH` to choose
  another. Check the GDB it will use, for example the default one:

  ```bash
  /usr/bin/gdb --batch -nx -ex 'python import gdb.dap.server'
  ```

  (or the `gdb` on `PATH` if there is no `/usr/bin/gdb`, or the path you
  will pass to `--gdb`).

  A version string alone does not establish a working DAP session.
- `readelf` and `setpriv` on `PATH` for reverse execution (managed replay
  refuses to start without them, and says so).
- The guest's `program` must have debug information, for example a C program
  built with `cc -g -O0`.

The examples below use this program, `squares.c`:

```c
#include <stdio.h>

static int square(int x) {
  int y = x * x;
  return y;
}

int main(void) {
  int total = 0;
  for (int i = 0; i < 3; i++) {
    total += square(i);
  }
  printf("total=%d\n", total);
  return 0;
}
```

```bash
cc -g -O0 -fno-pie -no-pie squares.c -o squares
```

## GDB

### On a deterministic run

`--gdbserver` makes Hermit serve the guest through the GDB remote protocol
and wait for a debugger before the guest's first instruction. It needs the
ptrace backend (the default) and switches the run to host networking, so
that GDB can reach the port; Hermit prints a warning when it does.

```bash
hermit run --epoch=2026-01-01T00:00:00Z --gdbserver --gdbserver-port=1234 -- ./squares
```

In another terminal:

```bash
gdb -ex 'set sysroot /' -ex 'target remote 127.0.0.1:1234' ./squares
(gdb) break square
(gdb) continue
```

`set sysroot /` makes GDB read shared libraries from the local file system
instead of requesting each one through the remote connection. With the same
`--epoch`, executable and input files, every rerun of this command stops at
the same breakpoint hits with the same values. A program that uses the
network is the exception: under `--gdbserver` it runs on the host network,
whose answers can change from run to run. Record such a program instead and
debug the recording.

### On a recording

```bash
hermit record start -- ./squares
hermit replay
```

`hermit record start` prints the recording's ID and the commands to replay
it. A bare `hermit replay` replays the latest recording and starts GDB
attached to it; pass an ID to choose another recording, `--data-dir` if the
recording was made with one, and `--gdbex 'CMD1;CMD2'` to give GDB commands
to run first. `hermit replay --autopilot` replays to the end without a
debugger.

GDB's own reverse commands (`reverse-step`, `reverse-continue`) do not work
on Hermit's remote target. Reverse execution is provided by `hermit-dap`,
below.

## Debug Adapter Protocol

`hermit-dap` is a Debug Adapter Protocol server for Hermit's GDB remote
target. It starts GDB's DAP interpreter (`gdb --interpreter=dap`), configured
so that GDB does not request shared libraries from Hermit's remote server.
Any DAP client can use it: an editor, [Dapper](https://github.com/facebookexperimental/dapper),
or a test harness.

The client sends an `attach` request naming the guest executable as
`program` and Hermit's GDB server as `target`:

```json
{"command": "attach", "arguments": {"program": "/path/to/squares", "target": "127.0.0.1:1234"}}
```

By default the client speaks DAP to `hermit-dap` on its stdin and stdout,
which is how editors usually run an adapter. With `--listen [HOST:]PORT`,
`hermit-dap` instead waits for one client on a TCP port, which is how a
client attaches to an adapter it did not start. `HOST` defaults to
`127.0.0.1`, and port 0 lets the system pick a free port. `hermit-dap`
writes `hermit-dap: listening on HOST:PORT` to stderr, serves the first
client that connects, and closes the port, so a second client is refused.
The session ends when that client disconnects.

Anyone who can connect to the port controls a debugger running as you, so
keep the default loopback address unless you mean otherwise. Hermit's GDB
server port is exposed the same way.

### Reverse execution on a recording

With `--replay ID`, `hermit-dap` starts and manages
`hermit replay ID --serve-only` itself, advertises `supportsStepBack`, and
implements DAP `stepBack` and `reverseContinue`. `--data-dir DIR` and
`--gdbserver-port PORT` (default 1234) are passed to that replay, and the
client's `attach` request names that port as its `target`.

The shortest way to start such a session is from `hermit replay`:

```bash
hermit record start -- ./squares
hermit replay --dap-listen=4711
```

```text
hermit replay: starting hermit-dap to serve recording cc474505... to one DAP client; its attach request needs "program": "/home/me/.cache/hermit/cc474505.../exe" and "target": "127.0.0.1:1234"
hermit-dap: listening on 127.0.0.1:4711
```

`--dap-listen` replaces the `hermit replay` process with
`hermit-dap --listen ADDRESS --replay ID`, passing on `--data-dir` and
`--gdbserver-port`. It cannot be combined with `--autopilot`,
`--serve-only` or `--gdbex`. Use the `program` it prints. For a native
executable that is the copy stored with the recording, whose debug
information points at the original source files. For a recorded script that
starts with `#!`, it is the script's interpreter, as for `hermit replay`
under GDB; reverse execution has been tested only on native C programs.

The same session over stdio, for a client that starts the adapter itself:

```bash
hermit-dap --replay ID --data-dir ~/.cache/hermit --gdbserver-port 1234
```

If the GDB in use lacks the DAP server internals that reverse execution
hooks, or `readelf` or `setpriv` is missing, managed replay refuses to
start: the client's first request gets a failed response naming the
problem, and the adapter writes the same message to its stderr and exits.

### How stepBack and reverseContinue work

With `--replay`, the adapter advertises `supportsStepBack` and implements DAP `stepBack` and `reverseContinue` by restarting the
deterministic replay and running forward to the requested earlier source
position. `stepBack` returns to the previous source line the replay passed or
the previous place the client stopped, whichever came later. When that place
does not start a source line, such as the return address a `stepOut` stops at,
`stepBack` restarts the replay once more to count the earlier passes through
it, and may first run the current replay forward to the next source line. This
first implementation favors correctness over speed.

`stepBack` goes back to the previous stop or line arrival, not to the previous
instruction GDB's own reverse-step would reach. When the client stops in a
one-line loop, for example by stepping out of the loop body's call, then
continues past the loop, `stepBack` returns to that stop. It skips the later
passes through the loop's line, which the client never stopped at and which
start no new source line. A `stepOut` from a function without debug
information reports the stop with reason `stopped` rather than `step`.

The adapter finds line arrivals with an internal breakpoint on every source
line, and chooses which arrival at an address to restart to by counting the
earlier ones. If the debugger stub loses one of those breakpoints, the count
is wrong and the replay runs to another arrival at the same address. Reverie's
gdbstub did lose them: it restored a whole 8-byte word when it removed a
software breakpoint, so when GDB removed a breakpoint to step over it, a
breakpoint up to 7 bytes after it was erased for the rest of that resume. That
was fixed in https://github.com/rrnewton/reverie/pull/890.

As a check on the count, the adapter records a frame identity with every stop
and line arrival: the program counter and stack pointer of the innermost
frame and of up to 7 of its callers, 8 frames at most (GDB does not unwind
past `main` by default, so a shallower stack gives fewer). When it replays to
a stop, it compares the identity where the replay landed. If they differ, it
puts the replay back at the client's current stop, sends a `stopped` event for
it, and fails the `stepBack` or `reverseContinue` with an error saying it
could not reach the earlier stop exactly. If it cannot put the replay back
either, the request fails and the adapter ends the session: it sends an
`output` event saying why, then `terminated`.

The frame identity tells apart two activations of a function at different
call depths, such as recursive calls, and two calls at the same depth from
different call sites, such as `g(1)` and `g(2)` on consecutive lines, as long
as the calling frame is among the 8. It cannot tell apart two passes through
the same activation, such as two iterations of a loop, two calls from the
same call site at the same depth, such as a call inside a loop, or two
arrivals whose 8 innermost frames are all equal and which differ only further
out, such as the same chain of 8 or more nested helper calls entered from
two call sites in one caller. Their identities are equal, so a wrong count
there would land at the wrong pass without an error. A test pins the limit:
with a chain of 7 helpers, the two activations differ only in the caller's
return address, frame 7, and a wrong count must be refused.

The identity is limited to 8 frames because the adapter takes it at every
line arrival while the replay runs forward, and unwinding every frame made
forward execution cost the square of the stack depth. Measured with GDB
17.2, one `continue` past a recursion 300 calls deep took 12.15 s
with every frame and 1.59 s with 8; past a recursion 1000 calls deep it took
121.79 s with every frame and 4.77 s with 8. A `stepBack` from there took
1.0 s at depth 300 and 1.3 s at depth 1000 either way.

## Driving a session with Dapper

[Dapper](https://github.com/facebookexperimental/dapper) is a DAP proxy with a
command-line client and an MCP server, so a person or an agent can drive a
session one request at a time. Its `from-config` mode starts a session from a
JSON file. This configuration connects to the `hermit replay --dap-listen=4711`
session above (replace RECORDING_ID and the paths):

```json
{
  "spawnConfig": {"type": "tcp", "addr": "127.0.0.1:4711"},
  "debugRequest": {
    "request": "attach",
    "program": "/home/me/.cache/hermit/RECORDING_ID/exe",
    "target": "127.0.0.1:1234"
  },
  "breakpoints": [{"type": "source", "path": "/path/to/squares.c", "line": 4}],
  "installDefaultExceptionBreakpoints": false
}
```

To have Dapper start the adapter over stdio instead, use
`"spawnConfig": {"type": "stdio", "cmd": "/path/to/hermit/target/debug/hermit-dap", "args": ["--replay", "RECORDING_ID", "--gdbserver-port", "1234"]}`,
adding `"--data-dir", "DIR"` for a recording made with one. For a
`hermit run --gdbserver` target, leave out `args`.

```bash
dapper proxy --control-port 4712 from-config hermit-dap.json
dapper debug --control-port 4712 continue 1
dapper debug --control-port 4712 continue 1
dapper debug --control-port 4712 stack-trace 1
dapper debug --control-port 4712 step back 1
dapper debug --control-port 4712 reverse-continue 1
```

With `squares`, the two `continue`s stop in `square` with `x = 1`, `step back`
returns to line 11 in `main` with `i = 1` and `total = 0`, and
`reverse-continue` returns to the first breakpoint hit, `x = 0`. Dapper's MCP
server offers the same operations to an agent (`debug_navigate_command` with
`step_back` and `reverse_continue`).

## VS Code

VS Code runs a debug adapter only for a debug type that some extension
contributes, so `hermit-dap` cannot be named in `launch.json` on its own. A
prototype extension in [editors/vscode](editors/vscode/README.md) contributes a
`hermit` debug type with two `attach` configurations: one that starts
`hermit-dap` on stdio against `hermit run --gdbserver`, and one that connects
to `hermit replay --dap-listen`, with Step Back and Reverse Continue.

Its test, `editors/vscode/test/run.sh`, drives both sessions through a real
VS Code (1.141.1) running headless, with no display: breakpoints, continue,
the call stack and variables, and on the recording `stepBack` and
`reverseContinue`. It also checks that VS Code's breakpoint list holds only
the breakpoint the test set. Nobody has yet used the extension in the VS Code
window itself, and it is not published.

What the extension relies on:

- `hermit-dap`, over stdio (`DebugAdapterExecutable`) or TCP (`--listen`,
  `DebugAdapterServer`). Its `attach` request needs only `program` and
  `target`.
- `hermit replay --dap-listen`, which starts a reverse-capable session from a
  terminal and prints the attach request's `program` and `target`.
- `hermit-dap` not reporting its hidden line breakpoints, so a client's
  breakpoint list holds only the breakpoints the user set.
- When a managed replay fails to start, for example on a recording made by an
  older Hermit, the client's error message includes what `hermit replay`
  said.

The intended workflow is to run `hermit run` or `hermit replay` in VS Code's
integrated terminal with one extra flag and have VS Code attach. A sketch of
the remaining design, not implemented:

1. The extension above provides the `hermit` debug type.
2. The terminal command (for example `hermit run --debug-ui -- PROGRAM`)
   starts the GDB server and a `hermit-dap --listen` on a free port, then
   asks VS Code to attach, for example by writing a session description to a
   directory the extension watches, or through a `vscode://` link the
   extension handles. In VS Code's terminal, including over Remote-SSH, the
   `VSCODE_IPC_HOOK_CLI` environment variable is set, so the command can tell
   that it runs inside VS Code.
3. VS Code becomes the debug client: breakpoints, the current line, the call
   stack, variables, and Step Back and Reverse Continue buttons for a
   recording.

### Watching an agent drive the debugger

Dapper can share one debug session between an editor and an agent. The
editor is the session's main client; an agent joins the same session through
Dapper's control plane (its command-line client or MCP server) and can set
breakpoints, step and inspect while the editor shows each stop. They take
turns. Dapper's documentation
describes wrapping an editor's adapter command in `dapper proxy process`;
`dapper proxy --client-port PORT` also accepts the editor's connection over
TCP. For Hermit this needs the VS Code debug type above. Conversation with
the agent happens in the agent's own interface, not in Dapper.

## Testing the adapter

The `hermit-dap` tests in `hermit-cli/tests/cli.rs` drive real DAP sessions
through a real GDB, with no editor: `initialize`, `attach`,
`setBreakpoints`, `configurationDone`, `continue`, `threads`, `stackTrace`,
`evaluate`, `next`, `stepOut`, `stepBack`,
`reverseContinue` and `disconnect`, over stdio and, through
`hermit replay --dap-listen`, over TCP.

```bash
HERMIT_REQUIRE_DAP=1 HERMIT_REQUIRE_DAP_GDB=1 \
  cargo nextest run -p hermit --test cli dap
```

Without `HERMIT_REQUIRE_DAP_GDB`, the end-to-end tests skip, loudly, when no
GDB with a DAP interpreter is installed. The 23 tests take about 7 to 10
seconds with GDB 17.2. The VS Code extension's test, which needs a VS Code
download and so is not part of validation, is described in
[editors/vscode](editors/vscode/README.md).

## Limitations

- Tested only on single-threaded C programs, with GDB 17.2.
- A `--listen` adapter serves one client and one session, then exits.
- Reverse execution is source-line granular and restarts the replay for each
  reverse request. See [How stepBack and reverseContinue work](#how-stepback-and-reversecontinue-work).
- `--gdbserver` and a gdb-served replay run on the host network, so their
  network behavior is not isolated or deterministic while debugging.
