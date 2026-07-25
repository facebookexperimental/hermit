# Hermit debugger integration tests (gdb / lldb)

These tests drive a **real debugger** against Hermit's built-in gdbserver to
exercise the interactive-debugging surface end to end: breakpoints, stepping
(in / over / out), variable & register inspection, and continue-to-exit, in both
`run` and `replay` modes.

## Layout

| File | What it covers |
| --- | --- |
| `guests/debuggee.c` | Tiny deterministic C guest with stable symbols/values. |
| `harness.py` | Shared, stdlib-only harness: locate/`build` hermit, compile the guest, start `hermit run --gdbserver`, wait for the port, record/replay helpers, precondition-based skipping. |
| `test_gdb_run_gdbserver.py` | GDB vs `hermit run --gdbserver`: breakpoint, step in/over/out, args/locals, `print` expr, registers, continue-to-exit. **Full assertions.** |
| `test_gdb_replay_gdbserver.py` | GDB vs `hermit replay`: deterministic replay — the debugger observes identical state (incl. a virtualized pid) across replays. **Full assertions.** |
| `test_lldb_run_gdbserver.py` | LLDB (Python API) vs `hermit run --gdbserver`: connect + plant breakpoint always asserted; stopped-state inspection asserted when available, else skipped with a reason. |
| `test_lldb_replay_gdbserver.py` | LLDB vs replay: documented skip until a serve-only replay mode exists (auto-activates when it does). |
| `run_debugger_tests.sh` | Standalone runner used by CI. |

## Running

```bash
tests/debugger/run_debugger_tests.sh            # everything
tests/debugger/run_debugger_tests.sh -v test_gdb_run_gdbserver
```

The runner builds `hermit` if needed and sets `PYTHONPATH="$(lldb -P)"` so the
`lldb` module imports. Every test **self-skips** when a prerequisite is missing
(no `hermit`, no `gdb`, no `lldb` module, or a host that cannot run Hermit —
e.g. no PMU / user namespaces), so it is safe to invoke unconditionally.

Override the binary with `HERMIT_BIN=/path/to/hermit`.

## Requirements

- A built `hermit` and a host that can run it (PMU + user namespaces).
- `gdb` on `PATH` for the GDB tests.
- The `lldb` Python module for the LLDB tests (`PYTHONPATH="$(lldb -P)"`).
- A C compiler (`cc`, or `$CC`) to build the guest (non-PIE, `-g -O0`).

## Known limitations (discovered while writing these tests)

1. **`finish` then `continue` panics reverie.** Issuing a step-out (`finish`)
   and then `continue` currently aborts the Hermit task with
   `unexpected resume action Continue(None), expecting: StepOver`
   (`reverie-ptrace/src/task.rs`). The tests therefore end step-out cases with
   `kill` instead of `continue`. This is a real Hermit/reverie bug worth a
   separate fix.

2. **LLDB stopped-state inspection is unavailable in some environments.** LLDB
   connects and plants breakpoints, but reading frames/registers/locals needs
   register-layout negotiation. When the LLDB build has XML target-description
   parsing disabled *and* Hermit's gdbserver lacks the `qRegisterInfo` fallback,
   LLDB cannot unwind (0 frames) and inspection is impossible. Those assertions
   skip with a precise reason and start passing automatically on a capable
   toolchain. GDB is unaffected and provides full inspection coverage.

3. **No serve-only replay mode.** `hermit replay` starts a gdbserver over the
   replay and immediately spawns its *own* gdb client, so an external LLDB has
   nothing to attach to. Deterministic-replay-under-a-debugger is covered via
   GDB (`--gdbex`). An external-LLDB-vs-replay test needs a future
   `hermit replay --gdbserver` (serve-and-wait) flag.

## CI

Wired into the self-hosted `hardware` job in `.github/workflows/ci.yml` (the
runner with PMU + user namespaces where Hermit integration tests already run),
gated on mount/user-namespace availability.
