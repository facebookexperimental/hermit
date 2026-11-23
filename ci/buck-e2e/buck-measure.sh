# Copyright (c) Meta Platforms, Inc. and affiliates.
# All rights reserved.
#
# This source code is licensed under the BSD-style license found in the
# LICENSE file in the root directory of this source tree.

# Record what a validation node's Buck build ran and what it cost, changing nothing it builds or
# decides. Sourced by the nodes that run buck2: ci/harvest-reverie-buck.sh (build.reverie_buck),
# ci/buck-e2e/measure-release-build (build.buck_release_artifact) and ci/buck-e2e/validate-node
# (the Buck E2E nodes). ci/buck-e2e/buck-measure-summarize.py types the facts into one sidecar per
# node, ${E2E_RESULT_ROOT}/buck-measure/<node>.json (target/validate/buck-measure/ outside a
# validation). <node> is the DAG step tag that dagrun exports (DAGRUN_OUTER_RUN), or the caller's
# name outside dagrun, so a sidecar always names the step it measured.
#
#   buck_measure_start NAME BUCK2 CHOSEN_BY  remove this node's earlier sidecar; begin a record
#   buck_measure_scope SCOPE                 the daemon runs in this systemd scope, outside the
#                                            node's cgroup (the stock runner, daemon-bound.sh's
#                                            bound_daemon): read its cpu.stat now
#   buck_measure_bootstrap NAME CMD...       run a preparation step, an executable (not a shell
#                                            function); record its exit status and CPU with
#                                            /usr/bin/time, or, without it, run it unmeasured.
#                                            Returns CMD's status.
#   buck_measure_build OUT ARGS...           `buck2 build --show-output ARGS >OUT`, with
#                                            --client-metadata id=hermit-buck-measure and, when
#                                            measuring, an event log. Returns buck2's status.
#   buck_measure_daemon [SECONDS]            while the daemon still runs: record where it runs,
#                                            from `buck2 status`, which starts none (and the
#                                            scope's end readings); SECONDS bounds the whole read
#   buck_measure_write [SECONDS]             write the sidecar
#   buck_measure_discard                     no sidecar this time: remove the work directory
#   bm_event_logs+=(LOG)                     another event log to count (.json-lines.gz, or the
#                                            plain JSON lines of `buck2 log show`)
#   bm_expected_event_logs=N                 how many such logs the node's invocations owe; fewer
#                                            whole logs make the actions unmeasured
#   bm_buck_out=DIR                          measure buck-out's bytes (unset: not measured)
#
# Where the daemon runs decides what it costs. A daemon that a node's client starts is forked in
# the node's cgroup, whose cpu.stat dagrun records as the step's CPU, and dagrun stops it with the
# node; its CPU is already the step's. Only the stock runner moves it to a systemd scope outside
# the node, whose CPU the sidecar then adds. Anything else is unmeasured, with the reason.
#
# Apart from the two that run the node's own commands, every function returns 0: a measurement
# that fails records why, or costs the sidecar, never the node. The comparison
# (ci/buck-build-compare.py) refuses a node without a whole sidecar.
# shellcheck shell=bash

bm_node='' bm_buck2='' bm_chosen_by='' bm_version='' bm_dir='' bm_work='' bm_logs=0
bm_scope='' bm_daemon_reason='' bm_show_output='' bm_buck_out='' bm_expected_event_logs=0
bm_event_logs=()
bm_here=$(dirname -- "${BASH_SOURCE[0]}")

bm_note() { echo "buck-measure: $*" >&2 || true; }

bm_read_version() { # buck2 --version, first line; empty when it cannot be read
    bm_version=''
    if [[ -n $bm_buck2 ]]; then
        bm_version=$(timeout --foreground -k 1 30 "$bm_buck2" --version 2>/dev/null) || bm_version=''
        bm_version=${bm_version%%$'\n'*}
    fi
    return 0
}

buck_measure_start() { # NAME BUCK2 CHOSEN_BY
    bm_node=${DAGRUN_OUTER_RUN:-$1} bm_buck2=$2 bm_chosen_by=$3
    bm_dir=${E2E_RESULT_ROOT:+$E2E_RESULT_ROOT/buck-measure}
    bm_dir=${bm_dir:-$PWD/target/validate/buck-measure}
    # An earlier attempt's sidecar must never pass for this attempt's.
    if ! mkdir -p -- "$bm_dir" 2>/dev/null || ! rm -f -- "$bm_dir/$bm_node.json" 2>/dev/null; then
        bm_note "cannot prepare $bm_dir/$bm_node.json; $bm_node writes no sidecar"
        bm_dir=''
        return 0
    fi
    if ! bm_work=$(mktemp -d "${TMPDIR:-/tmp}/buck-measure.XXXXXX" 2>/dev/null); then
        bm_note "cannot make a work directory; $bm_node writes no sidecar"
        bm_work='' bm_dir=''
        return 0
    fi
    if ! : 2>/dev/null >"$bm_work/bootstraps.tsv"; then
        bm_note "cannot write in $bm_work; $bm_node writes no sidecar"
        bm_work='' bm_dir=''
        return 0
    fi
    bm_read_version
    return 0
}

buck_measure_scope() { # SCOPE
    [[ -n $bm_work ]] || return 0
    if [[ $1 != /sys/fs/cgroup/*.scope || ! -r $1/cpu.stat || ! -r $1/memory.peak ]] ||
        ! cp -- "$1/cpu.stat" "$bm_work/cpu-start.stat" 2>/dev/null; then
        bm_daemon_reason="the daemon's systemd scope '$1' is not a readable cgroup v2 scope"
        return 0
    fi
    bm_scope=$1
    return 0
}

buck_measure_bootstrap() { # NAME CMD...: returns CMD's status
    local name=$1 rc=0 reading='' user='' system=''
    shift
    if [[ -z $bm_work || ! -x /usr/bin/time ]]; then
        "$@" || rc=$?
        if [[ -n $bm_work ]]; then
            printf '%s\t%s\tunmeasured\t%s\n' "$name" "$rc" "/usr/bin/time is not installed" \
                >>"$bm_work/bootstraps.tsv" 2>/dev/null || true
        fi
        return "$rc"
    fi
    /usr/bin/time -f '%U %S' -o "$bm_work/time.txt" "$@" || rc=$?
    # time(1) puts "Command exited with non-zero status N" before the reading: take the last line.
    reading=$(tail -n 1 -- "$bm_work/time.txt" 2>/dev/null) || reading=''
    read -r user system <<<"$reading" || true
    if [[ $user =~ ^[0-9]+(\.[0-9]+)?$ && $system =~ ^[0-9]+(\.[0-9]+)?$ ]]; then
        printf '%s\t%s\t%s\t%s\n' "$name" "$rc" "$user" "$system" >>"$bm_work/bootstraps.tsv" 2>/dev/null || true
    else
        printf '%s\t%s\tunmeasured\t%s\n' "$name" "$rc" "/usr/bin/time wrote no CPU reading" \
            >>"$bm_work/bootstraps.tsv" 2>/dev/null || true
    fi
    return "$rc"
}

buck_measure_build() { # OUT ARGS...: returns buck2's status
    local out=$1
    local -a log=()
    shift
    if [[ -n $bm_work ]]; then
        bm_logs=$((bm_logs + 1))
        log=(--event-log "$bm_work/build-$bm_logs.json-lines.gz")
        bm_show_output=$out
    fi
    "$bm_buck2" build "${log[@]}" --client-metadata id=hermit-buck-measure --show-output "$@" >"$out"
}

buck_measure_daemon() { # [SECONDS]
    [[ -n $bm_work ]] || return 0
    local limit=${1:-30} status=0 whole fraction='' deadline rest
    if [[ -n $bm_scope ]]; then
        if ! cp -- "$bm_scope/cpu.stat" "$bm_work/cpu-end.stat" 2>/dev/null ||
            ! cp -- "$bm_scope/memory.peak" "$bm_work/memory.peak" 2>/dev/null; then
            bm_daemon_reason="the daemon's systemd scope $bm_scope could not be read at the end"
        fi
        return 0
    fi
    # One budget for the whole read: the summarizer gets only what `buck2 status` left of it, and
    # nothing starts after it, so the read ends within SECONDS plus one kill grace. Its caller
    # stops the daemon next, inside a step's wall bound. Times are epoch microseconds; SECONDS is
    # whole or has up to six decimals. Only expansions and arithmetic here: no command runs.
    whole=${limit%%.*}
    [[ $limit != *.* ]] || fraction=${limit#*.}
    fraction=${fraction}000000
    deadline=$((${EPOCHREALTIME/[^0-9]/} + 10#${whole:-0} * 1000000 + 10#${fraction:0:6}))
    # `buck2 status` reaches only a running daemon. Its output also names the daemon's auth token:
    # the summarizer keeps the pids and cgroup paths alone, and the file is deleted at once.
    timeout --foreground -k 1 "$limit" "$bm_buck2" status >"$bm_work/status.json" 2>/dev/null || status=$?
    rest=$((deadline - ${EPOCHREALTIME/[^0-9]/}))
    if ((status != 0)); then
        bm_daemon_reason="\`buck2 status\` exited $status (124: it ran past $limit s)"
    elif ((rest <= 0)); then
        bm_daemon_reason="\`buck2 status\` used all $limit s, so where the daemon runs was not read"
    elif ! timeout --foreground -k 1 "$(printf '%d.%06d' $((rest / 1000000)) $((rest % 1000000)))" \
        python3 "$bm_here/buck-measure-summarize.py" daemon \
        --status "$bm_work/status.json" --out "$bm_work/daemon.json"; then
        bm_daemon_reason="where the daemon runs could not be read within $limit s"
    fi
    rm -f -- "$bm_work/status.json" 2>/dev/null || true
    return 0
}

buck_measure_write() { # [SECONDS]
    [[ -n $bm_work && -n $bm_dir ]] || return 0
    local limit=${1:-120} log status=0
    local -a args=(--node "$bm_node" --out "$bm_dir/$bm_node.json" --attempt "${DAGRUN_STEP_STARTED_MONOTONIC_NS:-}"
        --root "$PWD" --buck2 "$bm_buck2" --buck2-version "$bm_version" --buck2-chosen-by "$bm_chosen_by"
        --bootstraps "$bm_work/bootstraps.tsv" --expected-event-logs "$((bm_logs + bm_expected_event_logs))")
    if [[ -n $bm_daemon_reason ]]; then
        args+=(--daemon-unmeasured-reason "$bm_daemon_reason")
    elif [[ -n $bm_scope ]]; then
        args+=(--daemon-scope "$bm_scope" --scope-cpu-start "$bm_work/cpu-start.stat"
            --scope-cpu-end "$bm_work/cpu-end.stat" --scope-memory-peak "$bm_work/memory.peak")
    elif [[ -s $bm_work/daemon.json ]]; then
        args+=(--daemon-facts "$bm_work/daemon.json")
    fi
    for log in "$bm_work"/build-*.json-lines.gz "${bm_event_logs[@]}"; do
        if [[ -e $log ]]; then
            args+=(--event-log "$log")
        fi
    done
    if [[ -n $bm_show_output && -s $bm_show_output ]]; then
        args+=(--show-output "$bm_show_output")
    fi
    if [[ -n $bm_buck_out ]]; then
        args+=(--buck-out "$bm_buck_out")
    fi
    timeout --foreground -k 1 "$limit" python3 "$bm_here/buck-measure-summarize.py" "${args[@]}" || status=$?
    if ((status != 0)); then
        bm_note "$bm_node: no sidecar written (exit $status; 124: past the $limit s bound)"
    fi
    rm -rf -- "$bm_work" 2>/dev/null || true
    bm_work=''
    return 0
}

buck_measure_discard() { # no sidecar this time: remove the work directory, with any decoded logs
    [[ -n $bm_work ]] || return 0
    rm -rf -- "$bm_work" 2>/dev/null || true
    bm_work=''
    return 0
}
