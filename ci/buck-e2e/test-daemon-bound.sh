#!/usr/bin/env bash
# Copyright (c) Meta Platforms, Inc. and affiliates.
# All rights reserved.
#
# This source code is licensed under the BSD-style license found in the
# LICENSE file in the root directory of this source tree.

# Tests ci/buck-e2e/daemon-bound.sh against fake scope directories, with stubs for
# buck2 and systemctl: each of the CPU watcher's two breach paths (past the CPU
# budget; the scope gone) records the breach, stops the daemon (`buck2 kill`),
# while the scope exists kills it (cgroup.kill), and stops the node (SIGTERM to a
# stand-in node process); a scope under its budget, and its node, are left alone;
# record_daemon reaps the watcher before it returns, so removing the scope afterwards
# records no breach and leaves the node alone; kill_daemon_scope falls back to `systemctl --user kill` when cgroup.kill
# cannot be written; a failing `buck2 kill` before the start stops bound_daemon
# (exit 1) before it starts a daemon. Run by hand: ci/buck-e2e/test-daemon-bound.sh
set -euo pipefail
here=$(cd -- "$(dirname -- "${BASH_SOURCE[0]}")" && pwd)
tmp=$(mktemp -d)
trap 'rm -rf -- "$tmp"' EXIT
mkdir -p "$tmp/bin"
# The stubs expand $* and $CALLS when they run, not here.
# shellcheck disable=SC2016
printf '#!/usr/bin/env bash\necho "buck2 $*" >>"$CALLS"\n[[ $1 != kill ]] || exit "${BUCK2_KILL_STATUS:-0}"\n' >"$tmp/bin/buck2"
# shellcheck disable=SC2016
printf '#!/usr/bin/env bash\necho "systemctl $*" >>"$CALLS"\n' >"$tmp/bin/systemctl"
chmod +x "$tmp/bin/buck2" "$tmp/bin/systemctl"
export CALLS=$tmp/calls PATH=$tmp/bin:$PATH
buck2=$tmp/bin/buck2 state=$tmp/state kept=
daemon_memory_max=1 daemon_cpu_seconds=2 daemon_watch_interval=0.1
step() { :; }
# shellcheck source-path=SCRIPTDIR source=daemon-bound.sh
source "$here/daemon-bound.sh"

pass=0
fail() {
    echo "FAIL: $*" >&2
    exit 1
}
fake_scope() { # $1: a name. A scope directory at 1 CPU-s, as bound_daemon leaves it,
    # and a stand-in for the node's shell, which a breach signals.
    daemon_scope=$tmp/$1.scope daemon_scope_dir=$tmp/$1.scope daemon_unit=$1.scope
    sleep 60 &
    daemon_node_pid=$!
    mkdir -p "$daemon_scope_dir"
    echo 'usage_usec 1000000' >"$daemon_scope_dir/cpu.stat"
    : >"$daemon_scope_dir/cgroup.kill"
    daemon_cpu_start=1000000 daemon_breach=$tmp/$1.breach
    : >"$CALLS"
}
await_breach() { # $1: the watcher's pid. It records a breach and returns within 10 s.
    local i
    for ((i = 0; i < 100; i++)); do
        if [[ -s $daemon_breach ]] && ! kill -0 "$1" 2>/dev/null; then
            return 0
        fi
        sleep 0.1
    done
    kill "$1" 2>/dev/null || true
    fail "$daemon_unit: no breach recorded within 10 s"
}
node_end() { # how the stand-in node ended: "TERM", "alive" (then stopped here), or its status
    local status=0
    if kill -0 "$daemon_node_pid" 2>/dev/null && [[ $(ps -o stat= -p "$daemon_node_pid") != Z* ]]; then
        kill "$daemon_node_pid"
        wait "$daemon_node_pid" 2>/dev/null || true
        echo alive
        return
    fi
    wait "$daemon_node_pid" 2>/dev/null || status=$?
    if ((status == 143)); then echo TERM; else echo "status $status"; fi
}

fake_scope cpu
watch_daemon_cpu &
echo 'usage_usec 4000000' >"$daemon_scope_dir/cpu.stat"
await_breach $!
[[ $(<"$daemon_breach") == *"used 3 CPU-s, past its 2 CPU-s budget"* ]] || fail "cpu: breach record: $(<"$daemon_breach")"
[[ $(<"$CALLS") == "buck2 kill" ]] || fail "cpu: calls: $(<"$CALLS")"
[[ $(<"$daemon_scope_dir/cgroup.kill") == 1 ]] || fail "cpu: the scope was not killed"
[[ $(node_end) == TERM ]] || fail "cpu: the node was not signalled"
pass=$((pass + 1))

fake_scope gone
watch_daemon_cpu &
rm -r -- "$tmp/gone.scope"
await_breach $!
[[ $(<"$daemon_breach") == *"gone while the node runs"* ]] || fail "gone: breach record: $(<"$daemon_breach")"
[[ $(<"$CALLS") == "buck2 kill" ]] || fail "gone: calls (no systemctl for a gone scope): $(<"$CALLS")"
[[ $(node_end) == TERM ]] || fail "gone: the node was not signalled"
pass=$((pass + 1))

fake_scope quiet
watch_daemon_cpu &
watcher=$!
echo 'usage_usec 2900000' >"$daemon_scope_dir/cpu.stat"
sleep 1
kill "$watcher"
wait "$watcher" 2>/dev/null || true
[[ ! -e $daemon_breach && ! -s $CALLS && ! -s $daemon_scope_dir/cgroup.kill ]] ||
    fail "quiet: a scope under its budget was stopped: $(cat "$CALLS")"
[[ $(node_end) == alive ]] || fail "quiet: the node of a scope under its budget was signalled"
pass=$((pass + 1))

fake_scope record
echo 1 >"$daemon_scope_dir/memory.peak"
watch_daemon_cpu &
daemon_watch=$!
watcher=$daemon_watch
record_daemon 2>/dev/null
! kill -0 "$watcher" 2>/dev/null || fail "record: the watcher $watcher still exists after record_daemon (alive, or not reaped)"
[[ -z $daemon_watch ]] || fail "record: daemon_watch is still set"
rm -r -- "$tmp/record.scope"
sleep 0.5
[[ ! -e $daemon_breach && ! -s $CALLS ]] || fail "record: a breach after record_daemon: $(cat "$daemon_breach" "$CALLS" 2>/dev/null)"
[[ $(node_end) == alive ]] || fail "record: the node was signalled after record_daemon"
pass=$((pass + 1))

fake_scope readonly
chmod a-w "$daemon_scope_dir/cgroup.kill"
kill_daemon_scope
[[ $(<"$CALLS") == "systemctl --user kill -s KILL readonly.scope" ]] || fail "readonly: calls: $(<"$CALLS")"
node_end >/dev/null
pass=$((pass + 1))

: >"$CALLS"
status=0
out=$(BUCK2_KILL_STATUS=3 bound_daemon 2>&1) || status=$?
[[ $status == 1 && $out == *"buck2 kill"*"failed or ran past 60 s"* ]] || fail "kill fails: exit $status: $out"
[[ $(<"$CALLS") == "buck2 kill" ]] || fail "kill fails: calls (no daemon started after a failed kill): $(<"$CALLS")"
pass=$((pass + 1))

echo "test-daemon-bound.sh: $pass checks passed"
