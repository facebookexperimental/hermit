# Copyright (c) Meta Platforms, Inc. and affiliates.
# All rights reserved.
#
# This source code is licensed under the BSD-style license found in the
# LICENSE file in the root directory of this source tree.

# shellcheck shell=bash
# The caller defines buck2, state, kept, daemon_memory_max, daemon_cpu_seconds and
# step (validate-node; test-daemon-bound.sh), which shellcheck cannot see here.
# shellcheck disable=SC2154

# The buck2 daemon's bound in a Buck-only run (ci/buck-e2e/validate-node under
# buck-stock); sourced by validate-node and by ci/buck-e2e/test-daemon-bound.sh.
#
# The daemon, and every cell it runs, lives in its own systemd scope (buck2.slice),
# outside the node's cgroup and outside dagrun's reap: buck2 clears a test's
# environment, so no cell carries the node's DAGRUN_STEP. So the node bounds that
# scope itself: MemoryMax through systemd, a CPU budget watched every
# $daemon_watch_interval s, and at every end (exit, signal, breach) `buck2 kill`
# followed by the scope's cgroup.kill, so no cell outlives the node. A breach also
# signals the node (SIGTERM to daemon_node_pid), which stops at its next command
# boundary (bash runs the trap when the foreground command returns): the killed
# daemon fails the running buck2 client, and until the node exits, its next buck2
# command would start a daemon in a new scope, unbounded.
#
# Uses the caller's buck2, state, kept (empty for none), daemon_memory_max,
# daemon_cpu_seconds and step function.

# daemon_scope is cleared once its CPU and memory are recorded; daemon_scope_dir
# and daemon_unit stay, so the scope can still be killed.
daemon_scope='' daemon_scope_dir='' daemon_unit='' daemon_cpu_start='' daemon_watch='' daemon_breach='' daemon_node_pid=''
daemon_watch_interval=${daemon_watch_interval:-10}
bound_daemon() {
    local status_json memory_max kill_out
    daemon_breach=$state/daemon-bound-breach
    daemon_node_pid=$$ # the node's shell, which a breach signals
    mkdir -p "$state"
    rm -f -- "$daemon_breach"
    # A daemon started before this node (by hand, or by another run in this
    # checkout) carries no DAGRUN_STEP, no no-cargo shim on its PATH, and its own
    # environment: stop it, so the daemon this node bounds is one it starts.
    step "stopping any buck2 daemon already running for this checkout"
    if ! kill_out=$(timeout --foreground -k 5 60 "$buck2" kill 2>&1); then
        printf '%s\n' "$kill_out" >&2
        echo "validate-node: \`buck2 kill\` of the daemon already running for this checkout failed or ran past 60 s (above), so this node cannot start a daemon of its own to bound; nothing ran" >&2
        exit 1
    fi
    step "starting the buck2 daemon to bound its scope: MemoryMax $daemon_memory_max bytes, $daemon_cpu_seconds CPU-s"
    if ! status_json=$(timeout --foreground -k 5 600 "$buck2" server --status); then
        echo "validate-node: \`buck2 server --status\` failed or ran past 600 s (above), so no daemon was started and nothing ran" >&2
        exit 1
    fi
    daemon_scope=$(python3 -c 'import json, sys; print(json.load(sys.stdin).get("allprocs_cgroup_path") or "")' <<<"$status_json")
    if [[ $daemon_scope != /sys/fs/cgroup/*.scope || ! -r $daemon_scope/cpu.stat || ! -r $daemon_scope/memory.peak ]]; then
        echo "validate-node: the buck2 daemon has no systemd scope here ('${daemon_scope}', buck2_resource_control.status), so its CPU and memory, and the cells', cannot be bounded" >&2
        exit 2
    fi
    daemon_scope_dir=$daemon_scope
    daemon_unit=${daemon_scope##*/}
    if ! timeout --foreground -k 5 60 systemctl --user set-property --runtime "$daemon_unit" "MemoryMax=$daemon_memory_max"; then
        echo "validate-node: systemctl --user set-property --runtime $daemon_unit MemoryMax=$daemon_memory_max failed or took over 60 s (above); the daemon cannot be bounded, so no cell runs" >&2
        exit 1
    fi
    read -r memory_max <"$daemon_scope/memory.max"
    if [[ $memory_max != "$daemon_memory_max" ]]; then
        echo "validate-node: set MemoryMax=$daemon_memory_max on $daemon_unit, but its memory.max reads '${memory_max}'" >&2
        exit 2
    fi
    read -r _ daemon_cpu_start <"$daemon_scope/cpu.stat"
    watch_daemon_cpu &
    daemon_watch=$!
}
watch_daemon_cpu() { # every $daemon_watch_interval s: past the CPU budget, or with the scope gone, stop the daemon and kill the scope
    local usage budget_us=$((daemon_cpu_seconds * 1000000)) why
    while sleep "$daemon_watch_interval"; do
        if ! read -r _ usage 2>/dev/null <"$daemon_scope/cpu.stat"; then
            why="the bounded daemon's scope $daemon_scope is gone while the node runs: the daemon stopped, or was replaced by one without the bounds"
        elif ((usage - daemon_cpu_start > budget_us)); then
            why="the buck2 daemon's scope used $(((usage - daemon_cpu_start) / 1000000)) CPU-s, past its $daemon_cpu_seconds CPU-s budget"
        else
            continue
        fi
        echo "$why" >"$daemon_breach"
        echo "validate-node: $why: stopping the daemon and killing its scope" >&2
        timeout --foreground -k 1 30 "$buck2" kill >/dev/null 2>&1
        kill_daemon_scope
        # Stop the node too: until it exits, its next buck2 command would start a
        # daemon in a new scope, with no MemoryMax and no watcher.
        kill -s TERM "$daemon_node_pid" 2>/dev/null
        return
    done
}
kill_daemon_scope() { # after `buck2 kill`: kill what is left in the bounded scope, the cells the daemon ran included
    [[ -n $daemon_scope_dir ]] || return 0
    [[ -e $daemon_scope_dir/cgroup.kill ]] || return 0 # the scope is gone, and all that was in it
    # A builtin redirect: no fork, so this works on a host out of processes.
    if ! { echo 1 >"$daemon_scope_dir/cgroup.kill"; } 2>/dev/null; then
        timeout --foreground -k 1 10 systemctl --user kill -s KILL "$daemon_unit" >/dev/null 2>&1 ||
            echo "validate-node: could not kill what is left in $daemon_scope_dir (cgroup.kill, then systemctl --user kill $daemon_unit)" >&2
    fi
}
record_daemon() { # the scope's CPU and memory.peak, read with builtins (no fork) before the daemon stops
    local usage peak
    [[ -n $daemon_scope ]] || return 0
    # Stop the CPU watcher and reap it before the caller's `buck2 kill` removes the
    # scope: a watcher still running then would read the scope as gone, record a
    # breach and signal the node, failing a node that passed. wait fails (143, or
    # 127 once reaped), which must not stop the node under set -e.
    if [[ -n $daemon_watch ]]; then
        kill "$daemon_watch" 2>/dev/null
        wait "$daemon_watch" 2>/dev/null || true
        daemon_watch=
    fi
    if read -r _ usage 2>/dev/null <"$daemon_scope/cpu.stat" && read -r peak 2>/dev/null <"$daemon_scope/memory.peak"; then
        echo "validate-node: the buck2 daemon's scope used $(((usage - daemon_cpu_start) / 1000000)) of its $daemon_cpu_seconds CPU-s; memory.peak $peak of its MemoryMax $daemon_memory_max bytes" >&2
        [[ -z $kept ]] || printf 'scope\t%s\ncpu_usec\t%s\ncpu_budget_seconds\t%s\nmemory_peak_bytes\t%s\nmemory_max_bytes\t%s\n' \
            "$daemon_scope" "$((usage - daemon_cpu_start))" "$daemon_cpu_seconds" "$peak" "$daemon_memory_max" >"$kept/daemon-scope.tsv"
    else
        echo "validate-node: the buck2 daemon's scope $daemon_scope was gone before its CPU and memory.peak could be read" >&2
    fi
    daemon_scope=
}
