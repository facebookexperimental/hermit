#!/usr/bin/env bash
#
# Demo 2: record and replay.
#
# Record an execution into an isolated data directory, inspect the recording,
# and replay it to completion -- with and without GDB. Keep the recording
# directory, executable, inputs, and Hermit revision unchanged between recording
# and replay.

set -euo pipefail

# shellcheck source=demos/lib/display.sh
source "$(dirname "${BASH_SOURCE[0]}")/../lib/display.sh"

# shellcheck disable=SC2034  # consumed by common.sh demo_success/demo_failure
DEMO_LABEL="Demo 2: Record And Replay"
demo_header "$DEMO_LABEL"
echo 'Hermit records an execution into an isolated data directory, lists the recording'
echo 'in text and JSON, and replays it to completion with --autopilot. It can also'
echo 'record and immediately verify a replay. Without --autopilot, hermit replay'
echo 'starts a replay gdbserver and GDB client; the demo drives a noninteractive GDB'
echo 'session that continues the guest to completion. Keep the recording directory,'
echo 'executable, inputs, and Hermit revision unchanged between recording and replay.'
echo ''
echo '=========================================='

# shellcheck source=demos/lib/common.sh
source "$(dirname "${BASH_SOURCE[0]}")/../lib/common.sh"

export DEMO_DATA_DIR="$DEMO_TMP/recordings"
mkdir -p "$DEMO_DATA_DIR"

demo_banner "Record /bin/echo, list the recording, and replay it"
hermit --log=error record start \
  --data-dir="$DEMO_DATA_DIR" -- /bin/echo recorded
hermit record list --data-dir="$DEMO_DATA_DIR"
hermit record list --json --data-dir="$DEMO_DATA_DIR"
hermit --log=error replay --autopilot --data-dir="$DEMO_DATA_DIR"

demo_banner "Record and immediately verify a replay (temp recording auto-deleted)"
# --verify compares the deterministic execution log, which is empty at
# --log=error; hermit therefore requires --log=info (or more verbose) here.
hermit --log=info record start --verify \
  --data-dir="$DEMO_TMP/verified-recording" -- /bin/echo verified-recording

demo_banner "Replay under GDB (noninteractive: continue to completion)"
# Without --autopilot, replay starts a replay gdbserver and GDB client. This
# noninteractive session connects, continues the guest, and quits after
# /bin/echo completes. The trailing --gdbex=quit is required: once the guest
# exits, GDB has no more -ex commands to run and would otherwise drop to its
# interactive prompt and block on stdin, so `hermit replay` (which waits on the
# GDB client) would hang until the external timeout killed it. For interactive
# debugging, omit the --gdbex options and the external timeout.
timeout 90 hermit --log=error replay \
  --data-dir="$DEMO_DATA_DIR" \
  --gdbex='set confirm off' \
  --gdbex='set pagination off' \
  --gdbex=continue \
  --gdbex=quit

demo_success
