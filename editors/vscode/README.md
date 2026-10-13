# Hermit debug type for VS Code (prototype)

A minimal VS Code extension that contributes a `hermit` debug type, so VS Code
can debug a program under Hermit through `hermit-dap`, including Step Back and
Reverse Continue on a recording. It is a prototype: it is not published, and
it has been exercised only by the headless test below, not yet by a person in
the VS Code window. See [DEBUGGING.md](../../DEBUGGING.md) for the adapter
itself.

## Configurations

Both are `attach` configurations. `program` is the guest executable, which must
have debug information, and `target` is Hermit's GDB server.

Attach to `hermit run --gdbserver --gdbserver-port=1234 -- ./program`; VS Code
starts `hermit-dap` on stdio:

```json
{
  "type": "hermit",
  "request": "attach",
  "name": "Hermit run",
  "program": "${workspaceFolder}/program",
  "target": "127.0.0.1:1234",
  "dapPath": "/path/to/hermit/target/debug/hermit-dap"
}
```

Connect to `hermit replay --dap-listen=4711`, which prints the `program` and
`target` to use; this session supports Step Back and Reverse Continue:

```json
{
  "type": "hermit",
  "request": "attach",
  "name": "Hermit replay",
  "port": 4711,
  "program": "/home/me/.cache/hermit/RECORDING_ID/exe",
  "target": "127.0.0.1:1234"
}
```

Instead of `port`, `replay` (a recording ID), with optional `dataDir` and
`gdbserverPort`, makes VS Code start `hermit-dap --replay` on stdio.

To try it, open this directory's parent repository in VS Code and launch an
Extension Development Host with `--extensionDevelopmentPath=editors/vscode`,
or package it with `npx @vscode/vsce package`.

## Test

`test/run.sh` runs the extension in a real VS Code, headless (Electron's
`--ozone-platform=headless`, so no display or Xvfb is needed), through two
sessions on `test/fixtures/squares.c`:

- forward: VS Code starts `hermit-dap` on stdio against
  `hermit run --gdbserver`; two breakpoint stops in `square`, with `x = 0`
  and `x = 1`.
- reverse: VS Code connects to `hermit replay --dap-listen`; the same two
  stops, then Step Back to line 11 in `main` with `i = 1`, then Reverse
  Continue to `x = 0`.

Each also checks that VS Code's breakpoint list holds only the breakpoint the
test set, and that no DAP `breakpoint` event names one of hermit-dap's hidden
breakpoints. As a positive control for that check, each creates a breakpoint
from the debug console (`break FILE:13`, then `delete N`) and requires its
`new` and `removed` events. VS Code 1.141.1 does not add such
adapter-created breakpoints to its own breakpoint list; the test records
that rather than requiring it.

```bash
cargo build -p hermit --bin hermit --bin hermit-dap
# VS Code for Linux, unpacked from the .tar.gz at https://code.visualstudio.com/download
editors/vscode/test/run.sh --code /path/to/VSCode-linux-x64/code
```

It needs GDB with the DAP interpreter (tested with GDB 17.2), `cc` and
`node`. With VS Code 1.141.1 both sessions pass in about 75 seconds. Each VS
Code run is bounded by `--timeout` (default 300 s), and a session passes only
on the result file that run wrote, matched by a per-run nonce, with every
required step and no failures. The test is not part of Hermit's validation,
because it needs a VS Code download.

`test/selftest.sh` checks the harness itself without VS Code or Hermit: with
an editor that exits 0 without running the test, one that exits 1, and one
that hangs, each with a stale passing-looking result file left in the work
directory, `run.sh` must fail in bounded time (about 20 seconds in all).
