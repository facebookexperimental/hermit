#!/usr/bin/env bash
# Copyright (c) Meta Platforms, Inc. and affiliates.
# All rights reserved.
#
# This source code is licensed under the BSD-style license found in the
# LICENSE file in the root directory of this source tree.
#
# Standalone runner for the Hermit debugger (gdb / lldb) integration tests.
#
# It builds the hermit binary if needed, makes the `lldb` Python module
# importable (PYTHONPATH="$(lldb -P)"), and runs the stdlib-unittest suite in
# tests/debugger/. Tests self-skip when a prerequisite is missing (no hermit,
# no gdb, no lldb module, or a host that cannot run Hermit), so this is safe to
# invoke unconditionally in CI.
#
# Usage:
#   tests/debugger/run_debugger_tests.sh [unittest args...]
# Examples:
#   tests/debugger/run_debugger_tests.sh                 # all debugger tests
#   tests/debugger/run_debugger_tests.sh -v test_gdb_run_gdbserver

set -uo pipefail

HERE="$(cd -- "$(dirname -- "${BASH_SOURCE[0]}")" && pwd)"
ROOT="$(cd -- "$HERE/../.." && pwd)"
cd "$ROOT" || exit 1

# Locate (or build) the hermit binary.
if [ -z "${HERMIT_BIN:-}" ]; then
  if [ -x "$ROOT/target/debug/hermit" ]; then
    HERMIT_BIN="$ROOT/target/debug/hermit"
  elif [ -x "$ROOT/target/release/hermit" ]; then
    HERMIT_BIN="$ROOT/target/release/hermit"
  else
    echo ":: building hermit (cargo build -p hermit --bin hermit) ..."
    cargo build -p hermit --bin hermit || exit 1
    HERMIT_BIN="$ROOT/target/debug/hermit"
  fi
fi
export HERMIT_BIN
echo ":: HERMIT_BIN=$HERMIT_BIN"

# Ensure harness.py and the test modules are importable from any CWD.
export PYTHONPATH="${HERE}${PYTHONPATH:+:$PYTHONPATH}"

# Make the lldb Python module importable, if lldb is installed.
if command -v lldb >/dev/null 2>&1; then
  LLDB_PYPATH="$(lldb -P 2>/dev/null)"
  if [ -n "$LLDB_PYPATH" ]; then
    export PYTHONPATH="${LLDB_PYPATH}${PYTHONPATH:+:$PYTHONPATH}"
    echo ":: lldb python path: $LLDB_PYPATH"
  fi
else
  echo ":: lldb not found; lldb tests will skip"
fi

command -v gdb >/dev/null 2>&1 || echo ":: gdb not found; gdb tests will skip"

# Default: discover and run every test_*.py. Extra args override the selection.
if [ "$#" -eq 0 ]; then
  set -- discover -s "$HERE" -t "$HERE" -p 'test_*.py' -v
fi

exec python3 -m unittest "$@"
