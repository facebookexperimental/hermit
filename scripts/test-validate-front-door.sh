#!/usr/bin/env bash
# Copyright (c) Meta Platforms, Inc. and affiliates.
# All rights reserved.
#
# This source code is licensed under the BSD-style license found in the
# LICENSE file in the root directory of this source tree.

# Tests for scripts/validate-front-door.sh, the routing decision in front of
# `make validate`.
#
#   classifier  `--classify` over planted /proc trees and cleared
#               environments: each agent harness as an ancestor (by comm alone,
#               by comm and argv[0], by argv[0] alone, through an interpreter's
#               argv[1]), each environment marker, herdr agent and plain panes,
#               near-miss names, a PPid cycle, a vanished process, and a plain
#               human shell.
#   control     the whole suite rerun with TMPDIR inside a planted dev-hermit
#               parent, where a human's local run puts check.lint_checks; the
#               scratch root lives under /tmp so no case depends on TMPDIR.
#   routes      the real script in a planted dev-hermit parent whose
#               verify-submodules and driver are recording stubs: the agent
#               refusal (at most three lines, exact ci-hub command, nothing
#               run), the human route on a clean and a dirty tree, the ci-hub
#               unit and standalone pass-throughs, section headers, exit
#               status propagation, the unpopulated-reverie refusal, and that
#               no route runs verify-submodules in front of the driver (the
#               DAG's pre.submodules node owns that check).
#   cost        the decision is measured in CPU seconds (load-invariant, so it
#               holds on a loaded host), never in wall time.
#   fetch       before the driver is exec'd, a failed fetch of its Cargo
#               dependencies stops the run (exit 75) naming Cargo's words and
#               the remedy, and the driver never starts; a TLS failure names
#               CARGO_NET_GIT_FETCH_WITH_CLI; a fetch past its WALL bound is
#               named as such.
#   make        `make validate` (dry run) hands the front door
#               --all-unless-level and then ARGS unchanged, so the driver's own
#               parser decides whether ARGS names a level.
#
# The human route needs a caller with no agent ancestor even when an agent runs
# this test, so those cases detach with `setsid -f` under `env -i`: the child
# is re-parented to the nearest subreaper. If that subreaper is itself an agent,
# the cases are SKIPPED BY NAME rather than passed.
#
# `--e2e [DRIVER_ARGS...]` instead runs the REAL front door on this checkout's
# REAL driver, detached the same way: the human route end to end, compiling
# the driver and running the DAG off the record (default `--quick`, which
# builds Hermit; `--only portable pre.submodules` runs just the portable
# preflight: the checkout gates, the rust-script and manifest-plan builds and
# gate.manifest). It is deliberately not part of the default run, which stays
# cheap.
#
# Exit status: 0 every case passed (named skips allowed); 1 a case failed;
# 2 usage error.

set -euo pipefail

ROOT=$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd -P)
# A mutant can be substituted to show that these cases bite.
FRONT_DOOR=${VALIDATE_FRONT_DOOR_UNDER_TEST:-$ROOT/scripts/validate-front-door.sh}
MAKEFILE=${VALIDATE_MAKEFILE_UNDER_TEST:-$ROOT/Makefile}
# The CPU budget for one routing decision. Measured 0.06 CPU s at load ~190
# (host in docs/TESTING_ENVIRONMENTS.md, "Named measurement hosts"); a compile
# of the driver costs 100+ CPU seconds.
readonly DECISION_CPU_BUDGET=0.5
readonly DETACHED_WALL_BACKSTOP=60

# The scratch root lives directly under /tmp and never under TMPDIR. The front
# door's find_parent walks every ancestor of a planted checkout, and the driver
# points an unset TMPDIR inside the tree it runs in
# (target/validation/run-*/tmp). Under TMPDIR, the standalone fixture would
# find the real dev-hermit parent there and be refused: the human route's own
# check.lint_checks went red that way (review of 922b81d6, 2026-11-21).
# control_cases reruns this suite with TMPDIR inside a planted parent to keep it
# that way.
TMP=$(mktemp -d /tmp/validate-front-door-test.XXXXXX)
trap 'rm -rf -- "$TMP"' EXIT

# The same predicate as the front door's find_parent: a .gitmodules declaring
# the hermit submodule at path "hermit". Prints the first such ancestor of DIR.
dev_hermit_ancestor() {
    local dir=$1
    while [[ -n $dir && $dir != / ]]; do
        if [[ -f $dir/.gitmodules ]] \
            && grep -Eq '^[[:space:]]*path[[:space:]]*=[[:space:]]*hermit[[:space:]]*$' \
                "$dir/.gitmodules"; then
            printf '%s\n' "$dir"
            return 0
        fi
        dir=${dir%/*}
    done
    return 1
}

if found_parent=$(dev_hermit_ancestor "$TMP"); then
    printf 'FAIL scratch root %s lies inside the dev-hermit parent %s, so the standalone cases cannot be hermetic\n' \
        "$TMP" "$found_parent"
    exit 1
fi

cases=0
failures=0
skips=0

pass() {
    cases=$((cases + 1))
    printf 'ok   %s\n' "$1"
}

fail() {
    cases=$((cases + 1))
    failures=$((failures + 1))
    printf 'FAIL %s\n' "$1"
    shift
    local line
    for line in "$@"; do
        printf '       %s\n' "$line"
    done
}

skip() {
    skips=$((skips + 1))
    printf 'SKIP %s: %s\n' "$1" "$2"
}

# A cleared environment: no agent markers, no herdr pane, no parent override.
clean_env=(env -i "PATH=$PATH" "HOME=${HOME:-/nonexistent}" "LC_ALL=C")

# ---------------------------------------------------------------- classifier

# mkproc ROOT PID PPID COMM [ARGV...]
mkproc() {
    local root=$1 pid=$2 ppid=$3 comm=$4
    shift 4
    mkdir -p "$root/$pid"
    printf '%s\n' "$comm" >"$root/$pid/comm"
    printf 'Name:\t%s\nState:\tS (sleeping)\nPPid:\t%s\n' "$comm" "$ppid" >"$root/$pid/status"
    if (($# > 0)); then
        printf '%s\0' "$@" >"$root/$pid/cmdline"
    else
        printf '%s\0' "$comm" >"$root/$pid/cmdline"
    fi
}

fake_herdr() {
    local path=$1 reply=$2
    {
        printf '#!/usr/bin/env bash\n'
        printf 'printf "%%s\\n" %q\n' "$reply"
    } >"$path"
    chmod +x "$path"
}

# expect_class NAME EXPECTED_LINE CMD...
expect_class() {
    local name=$1 want=$2 got
    shift 2
    if ! got=$("$@" 2>&1); then
        fail "$name" "classifier exited nonzero: $got"
    elif [[ $got == "$want" ]]; then
        pass "$name"
    else
        fail "$name" "want: $want" "got:  $got"
    fi
}

readonly HUMAN_LINE='human no agent ancestor, agent environment marker or herdr agent pane'

classify_with() {
    local proc=$1 start=$2
    shift 2
    "${clean_env[@]}" "$@" bash "$FRONT_DOOR" --classify --proc-root "$proc" \
        --start-pid "$start" --herdr "$TMP/no-herdr"
}

classifier_cases() {
    local proc=$TMP/proc-human name
    mkproc "$proc" 1 0 systemd
    mkproc "$proc" 200 1 sshd /usr/sbin/sshd -D
    mkproc "$proc" 300 200 bash -bash
    expect_class "classify: a plain human shell" "$HUMAN_LINE" classify_with "$proc" 300

    for name in claude muse codex agentctl; do
        proc=$TMP/proc-$name
        mkproc "$proc" 1 0 systemd
        mkproc "$proc" 200 1 bash bash
        mkproc "$proc" 250 200 "$name" "/usr/local/bin/$name"
        mkproc "$proc" 300 250 bash bash
        mkproc "$proc" 400 300 make make validate
        expect_class "classify: $name as an ancestor (comm and argv[0])" \
            "agent ancestor process 250 is $name" classify_with "$proc" 400
    done

    # Only the comm check can see these: argv[0] names an interpreter and
    # argv[1] a script whose basename is no harness, as when a launcher sets
    # its process name but runs from a bundled script.
    for name in claude muse codex agentctl; do
        proc=$TMP/proc-comm-only-$name
        mkproc "$proc" 1 0 systemd
        mkproc "$proc" 250 1 "$name" node /opt/bundle/cli.js --serve
        mkproc "$proc" 300 250 bash bash
        expect_class "classify: $name known only by comm" \
            "agent ancestor process 250 is $name" classify_with "$proc" 300
    done

    proc=$TMP/proc-node
    mkproc "$proc" 1 0 systemd
    mkproc "$proc" 250 1 node node /opt/tools/bin/claude --resume
    mkproc "$proc" 300 250 bash bash
    expect_class "classify: an interpreter running claude (argv[1])" \
        "agent ancestor process 250 is claude" classify_with "$proc" 300

    proc=$TMP/proc-argv0
    mkproc "$proc" 1 0 systemd
    mkproc "$proc" 250 1 MainThread /usr/local/bin/codex_cli/codex exec
    mkproc "$proc" 300 250 bash bash
    expect_class "classify: a renamed thread whose argv[0] is codex" \
        "agent ancestor process 250 is codex" classify_with "$proc" 300

    proc=$TMP/proc-nearmiss
    mkproc "$proc" 1 0 systemd
    mkproc "$proc" 240 1 claudette claudette
    mkproc "$proc" 250 240 muse-player /usr/bin/muse-player --codex
    mkproc "$proc" 300 250 bash bash
    expect_class "classify: near-miss names are not agents" "$HUMAN_LINE" \
        classify_with "$proc" 300

    proc=$TMP/proc-cycle
    mkproc "$proc" 300 301 bash bash
    mkproc "$proc" 301 300 bash bash
    expect_class "classify: a PPid cycle terminates" "$HUMAN_LINE" classify_with "$proc" 300

    proc=$TMP/proc-gone
    mkdir -p "$proc"
    expect_class "classify: a vanished start process" "$HUMAN_LINE" classify_with "$proc" 300

    proc=$TMP/proc-human
    for name in CLAUDECODE AI_AGENT AGENT CODING_AGENT_METADATA META_3PAI_AGENT_PLATFORM \
        MUSE_SESSION_ID CODEX_THREAD_ID CODEX_SANDBOX DG_AGENT_NAME AGENTCTL_AGENT; do
        expect_class "classify: environment marker $name" \
            "agent environment marker $name is set" classify_with "$proc" 300 "$name=1"
    done
    expect_class "classify: an empty marker is not a marker" "$HUMAN_LINE" \
        classify_with "$proc" 300 "CLAUDECODE="

    fake_herdr "$TMP/herdr-agent" '{"result":{"pane":{"agent":"muse","agent_status":"idle","pane_id":"wX:p1"}}}'
    fake_herdr "$TMP/herdr-plain" '{"result":{"pane":{"agent":null,"agent_status":null,"pane_id":"wX:p2"}}}'
    expect_class "classify: a herdr agent pane" "agent herdr pane wX:p1 hosts a muse agent" \
        "${clean_env[@]}" HERDR_PANE_ID=wX:p1 bash "$FRONT_DOOR" --classify \
        --proc-root "$proc" --start-pid 300 --herdr "$TMP/herdr-agent"
    expect_class "classify: a plain herdr pane" "$HUMAN_LINE" \
        "${clean_env[@]}" HERDR_PANE_ID=wX:p2 bash "$FRONT_DOOR" --classify \
        --proc-root "$proc" --start-pid 300 --herdr "$TMP/herdr-plain"
    expect_class "classify: an unreachable herdr does not classify" "$HUMAN_LINE" \
        "${clean_env[@]}" HERDR_PANE_ID=wX:p1 bash "$FRONT_DOOR" --classify \
        --proc-root "$proc" --start-pid 300 --herdr "$TMP/no-such-herdr"
    expect_class "classify: herdr is not asked outside a pane" "$HUMAN_LINE" \
        "${clean_env[@]}" bash "$FRONT_DOOR" --classify \
        --proc-root "$proc" --start-pid 300 --herdr "$TMP/herdr-agent"

    local out rc=0
    out=$("${clean_env[@]}" bash "$FRONT_DOOR" --proc-root "$proc" -- --all 2>&1) || rc=$?
    if ((rc == 2)) && [[ $out == *"accepted only with --classify"* ]]; then
        pass "classify: test seams are refused outside --classify"
    else
        fail "classify: test seams are refused outside --classify" "rc=$rc" "$out"
    fi
}

# cpu_seconds CMD... -> prints user+sys CPU seconds of CMD (bash `time`).
cpu_seconds() {
    local timing TIMEFORMAT='%U %S'
    timing=$({ time "$@" >/dev/null 2>&1; } 2>&1) || true
    awk '{ printf "%.3f\n", $1 + $2 }' <<<"$timing"
}

within_budget() {
    awk -v got="$1" -v cap="$DECISION_CPU_BUDGET" 'BEGIN { exit !(got < cap) }'
}

cost_cases() {
    local cpu
    cpu=$(cpu_seconds bash "$FRONT_DOOR" --classify)
    if within_budget "$cpu"; then
        pass "cost: classifying this real process tree took $cpu CPU s (< $DECISION_CPU_BUDGET)"
    else
        fail "cost: classifying this real process tree" "took $cpu CPU s, budget $DECISION_CPU_BUDGET"
    fi
}

# -------------------------------------------------------------------- routes

# A planted dev-hermit parent: ci-hub/, a .gitmodules naming the hermit
# submodule, and a git checkout at hermit/ whose verify-submodules and driver
# record what they were given, with a populated-looking reverie.
plant_checkout() {
    local parent=$1 with_boundary=$2 hermit=$1/hermit
    mkdir -p "$hermit/scripts" "$hermit/ci" "$hermit/reverie/reverie"
    printf '[package]\nname = "reverie-core"\n' >"$hermit/reverie/reverie/Cargo.toml"
    if [[ $with_boundary == yes ]]; then
        mkdir -p "$parent/ci-hub"
        printf '[submodule "hermit"]\n\tpath = hermit\n\turl = https://example.invalid/hermit\n' \
            >"$parent/.gitmodules"
    fi
    cp "$FRONT_DOOR" "$hermit/scripts/validate-front-door.sh"
    cat >"$hermit/scripts/validate.rs" <<'STUB'
#!/usr/bin/env bash
printf '%s\n' "$@" >"$STUB_LOG/driver"
printf '%s\n' "${TMPDIR:-}" >"$STUB_LOG/tmpdir"
if [[ -n ${TMPDIR:-} && -d $TMPDIR ]]; then printf 'yes\n' >"$STUB_LOG/tmpdir-existed"; fi
printf '%s\n' "$$" >"$STUB_LOG/pid"
if [[ -n ${STUB_PROXY_MARK:-} ]]; then printf '%s\n' "$STUB_PROXY_MARK" >"$STUB_LOG/proxy"; fi
if [[ -n ${STUB_SPAWN_CHILD:-} ]]; then
    # Leave a child behind that outlives this driver, holding every fd the
    # driver inherited (the ownership lock among them), until the test
    # releases it (bounded: 60 s WALL).
    (
        : >"$STUB_LOG/child-ready"
        for _ in $(seq 1 600); do
            [[ -e $STUB_LOG/child-release ]] && break
            sleep 0.1
        done
        : >"$STUB_LOG/child-done"
    ) </dev/null >/dev/null 2>&1 &
    for _ in $(seq 1 100); do [[ -e $STUB_LOG/child-ready ]] && break; sleep 0.1; done
fi
if [[ -n ${STUB_WAIT_FOR_TERM:-} ]]; then
    # Wait, bounded, for the test to send SIGTERM to this process; record
    # whether the private TMPDIR still existed when it arrived.
    trap 'if [[ -d $TMPDIR ]]; then printf "yes\n" >"$STUB_LOG/tmpdir-at-term"; fi; exit 143' TERM
    : >"$STUB_LOG/ready"
    for _ in $(seq 1 600); do sleep 0.1; done
    exit 99
fi
exit "${STUB_DRIVER_EXIT:-0}"
STUB
    cat >"$hermit/ci/verify-submodules.sh" <<'STUB'
#!/usr/bin/env bash
printf 'SUBMODULE_GIT=%s\n' "${SUBMODULE_GIT:-}" >"$STUB_LOG/submodules"
exit "${STUB_SUBMODULES_EXIT:-0}"
STUB
    chmod +x "$hermit/scripts/validate-front-door.sh" "$hermit/scripts/validate.rs" \
        "$hermit/ci/verify-submodules.sh"
    printf 'tracked\n' >"$hermit/tracked.txt"
    git -C "$hermit" init -q
    git -C "$hermit" add -A
    git -C "$hermit" -c user.name=front-door-test -c user.email=test@example.invalid \
        commit -q -m planted
}

new_log() {
    local log
    log=$(mktemp -d "$TMP/log.XXXXXX")
    printf '%s\n' "$log"
}

# The private TMPDIR is removed by a detached reaper after the driver exits;
# wait for that, bounded (WALL backstop; the reaper's own work takes
# milliseconds).
readonly REAPER_WALL_BACKSTOP_TENTHS=150
wait_gone() {
    local path=$1 tenth
    for ((tenth = 0; tenth < REAPER_WALL_BACKSTOP_TENTHS; tenth++)); do
        [[ -e $path ]] || return 0
        sleep 0.1
    done
    return 1
}

# A with-proxy stand-in first on PATH: it marks the environment it hands on,
# as the real one sets proxy variables, and runs its arguments.
plant_with_proxy() {
    local bin=$1
    mkdir -p "$bin"
    printf '#!/usr/bin/env bash\nexport STUB_PROXY_MARK=with-proxy\nexec "$@"\n' >"$bin/with-proxy"
    chmod +x "$bin/with-proxy"
}

line_count() {
    local count=0 line
    while IFS= read -r line; do count=$((count + 1)); done <"$1"
    printf '%s\n' "$count"
}

# run_detached LOG OUTFILE ENVS... -- CMD...: run CMD under env -i, detached
# from this process tree, and print its exit status.
run_detached() {
    local log=$1 out=$2 fifo fd rc line
    shift 2
    local -a envs=()
    while (($# > 0)) && [[ $1 != -- ]]; do
        envs+=("$1")
        shift
    done
    shift
    fifo=$log/done.fifo
    mkfifo "$fifo"
    exec {fd}<>"$fifo"
    setsid -f env -i "PATH=$PATH" "HOME=${HOME:-/nonexistent}" "LC_ALL=C" \
        "OUT=$out" "FIFO=$fifo" "${envs[@]}" \
        bash -c '"$@" >"$OUT" 2>&1; printf "%s\n" "$?" >"$FIFO"' front-door-detached "$@" \
        2>/dev/null || true
    if read -r -t "$DETACHED_WALL_BACKSTOP" line <&"$fd"; then
        rc=$line
    else
        rc="timeout after ${DETACHED_WALL_BACKSTOP} s WALL"
    fi
    exec {fd}<&-
    printf '%s\n' "$rc"
}

route_cases() {
    local parent=$TMP/parent hermit sha log out rc
    plant_checkout "$parent" yes
    hermit=$parent/hermit
    sha=$(git -C "$hermit" rev-parse HEAD)

    # R1: an agent ancestor. A bash named `claude` runs the front door as its
    # child, so the refusal must come from the ancestry walk alone.
    mkdir -p "$TMP/bin"
    ln -s "$(command -v bash)" "$TMP/bin/claude"
    log=$(new_log)
    out=$log/out
    rc=0
    "${clean_env[@]}" STUB_LOG="$log" "$TMP/bin/claude" -c '"$0" -- --all; exit $?' \
        "$hermit/scripts/validate-front-door.sh" >"$out" 2>&1 || rc=$?
    local want_cmd
    want_cmd="  $parent/ci-hub/ci-hub validate-run --checkout $hermit --agent '<registered-agent-name>' --target $sha -- --all"
    if ((rc == 75)) && (($(line_count "$out") <= 3)) && grep -Fxq -- "$want_cmd" "$out" \
        && grep -q '(ancestor process [0-9]* is claude)' "$out" \
        && [[ ! -e $log/driver && ! -e $log/submodules ]]; then
        pass "route: an agent ancestor is refused in <= 3 lines with the exact ci-hub command"
    else
        fail "route: an agent ancestor is refused" "rc=$rc (want 75)" \
            "driver ran: $([[ -e $log/driver ]] && echo yes || echo no)" \
            "submodules ran: $([[ -e $log/submodules ]] && echo yes || echo no)" \
            "want line: $want_cmd" "$(cat "$out")"
    fi

    # R2: an environment marker, with the lane's registered name.
    log=$(new_log)
    out=$log/out
    rc=0
    "${clean_env[@]}" STUB_LOG="$log" CLAUDECODE=1 DG_AGENT_NAME=lane-x \
        "$hermit/scripts/validate-front-door.sh" -- --all --verbose >"$out" 2>&1 || rc=$?
    if ((rc == 75)) && (($(line_count "$out") <= 3)) \
        && grep -Fq -- "--agent lane-x --target $sha -- --all --verbose" "$out" \
        && grep -Fq 'environment marker CLAUDECODE is set' "$out" && [[ ! -e $log/driver ]]; then
        pass "route: an environment marker is refused, naming the lane and its arguments"
    else
        fail "route: an environment marker is refused" "rc=$rc" "$(cat "$out")"
    fi

    local cpu
    cpu=$(cpu_seconds "${clean_env[@]}" STUB_LOG="$log" CLAUDECODE=1 \
        "$hermit/scripts/validate-front-door.sh" -- --all)
    if within_budget "$cpu"; then
        pass "cost: the whole agent refusal took $cpu CPU s (< $DECISION_CPU_BUDGET)"
    else
        fail "cost: the whole agent refusal" "took $cpu CPU s, budget $DECISION_CPU_BUDGET"
    fi

    # R3: inside a ci-hub validate-run unit the driver's admission decides,
    # even when an agent marker leaked in; the unit's own TMPDIR (outside the
    # checkout, as start_unit gives it) reaches the driver unchanged.
    local unit_tmp=$TMP/unit-tmp proxybin=$TMP/proxybin
    mkdir -p "$unit_tmp"
    plant_with_proxy "$proxybin"
    log=$(new_log)
    out=$log/out
    rc=0
    "${clean_env[@]}" PATH="$proxybin:$PATH" STUB_LOG="$log" CLAUDECODE=1 \
        CI_HUB_VALIDATE_PRODUCER=systemd-user-v1 TMPDIR="$unit_tmp" \
        "$hermit/scripts/validate-front-door.sh" -- --all >"$out" 2>&1 || rc=$?
    if ((rc == 0)) && [[ $(cat "$log/driver" 2>/dev/null) == "--all" ]] \
        && [[ ! -e $log/submodules ]] \
        && [[ $(cat "$log/tmpdir" 2>/dev/null) == "$unit_tmp" ]] \
        && [[ ! -e $log/proxy ]] \
        && grep -q 'inside a ci-hub validate-run unit' "$out"; then
        pass "route: a ci-hub unit starts the driver unchanged, with no pre-gate in front"
    else
        fail "route: a ci-hub unit passes through" "rc=$rc" \
            "driver argv: $(tr '\n' ' ' <"$log/driver" 2>/dev/null)" \
            "driver TMPDIR: $(cat "$log/tmpdir" 2>/dev/null) (want $unit_tmp)" \
            "verify-submodules ran in front: $([[ -e $log/submodules ]] && echo yes || echo no)" \
            "$(cat "$out")"
    fi

    # R3b: a TMPDIR inside the checkout would put the driver's scratch inside
    # the Cargo workspace, which its self-test node refuses, so the front door
    # substitutes a private directory under /tmp and removes it afterwards.
    log=$(new_log)
    out=$log/out
    rc=0
    "${clean_env[@]}" STUB_LOG="$log" CI_HUB_VALIDATE_PRODUCER=systemd-user-v1 \
        TMPDIR="$hermit/target/tmp" \
        "$hermit/scripts/validate-front-door.sh" -- --all >"$out" 2>&1 || rc=$?
    local seen_tmp header_out=$out
    seen_tmp=$(cat "$log/tmpdir" 2>/dev/null || true)
    if ((rc == 0)) && [[ $seen_tmp == /tmp/hermit-validate-?????? ]] \
        && [[ -e $log/tmpdir-existed ]] && wait_gone "$seen_tmp"; then
        pass "route: a TMPDIR inside the checkout is replaced by a private /tmp directory, removed afterwards"
    else
        fail "route: an in-checkout TMPDIR is replaced" "rc=$rc" "driver TMPDIR: $seen_tmp" \
            "existed during the run: $([[ -e $log/tmpdir-existed ]] && echo yes || echo no)" \
            "still present: $([[ -n $seen_tmp && -e $seen_tmp ]] && echo yes || echo no)" "$(cat "$out")"
    fi

    # R3c: the driver is exec'd, so the process the caller started IS the
    # driver: a SIGTERM sent to it reaches the driver directly, the private
    # TMPDIR still exists while the driver handles it, and the reaper removes it
    # only after the driver has exited.
    local sig_log front_pid driver_pid tenth
    sig_log=$(new_log)
    "${clean_env[@]}" STUB_LOG="$sig_log" STUB_WAIT_FOR_TERM=1 \
        CI_HUB_VALIDATE_PRODUCER=systemd-user-v1 TMPDIR="$hermit/target/tmp" \
        "$hermit/scripts/validate-front-door.sh" -- --all >"$sig_log/out" 2>&1 &
    front_pid=$!
    tenth=0
    while [[ ! -e $sig_log/ready ]] && ((tenth < REAPER_WALL_BACKSTOP_TENTHS)); do
        sleep 0.1
        tenth=$((tenth + 1))
    done
    driver_pid=$(cat "$sig_log/pid" 2>/dev/null || true)
    seen_tmp=$(cat "$sig_log/tmpdir" 2>/dev/null || true)
    local present_before='no'
    [[ -n $seen_tmp && -d $seen_tmp ]] && present_before='yes'
    kill -TERM "$front_pid" 2>/dev/null || true
    rc=0
    wait "$front_pid" || rc=$?
    if [[ -e $sig_log/ready ]] && [[ $driver_pid == "$front_pid" ]] && ((rc == 143)) \
        && [[ $present_before == yes && -e $sig_log/tmpdir-at-term ]] \
        && [[ $seen_tmp == /tmp/hermit-validate-?????? ]] && wait_gone "$seen_tmp"; then
        pass "route: the driver is exec'd; its SIGTERM keeps the private TMPDIR until it exits, then the reaper removes it"
    else
        fail "route: exec and signal handling" "rc=$rc (want 143)" \
            "front-door pid $front_pid, driver pid ${driver_pid:-none} (want the same)" \
            "TMPDIR $seen_tmp: present before TERM $present_before; at TERM $([[ -e $sig_log/tmpdir-at-term ]] && echo yes || echo no); still present $([[ -n $seen_tmp && -e $seen_tmp ]] && echo yes || echo no)" \
            "$(cat "$sig_log/out")"
    fi

    # R3d: a TMPDIR that resolves inside the checkout through a symlink, or
    # given relative, is replaced too; the comparison is realpath to realpath.
    ln -s "$hermit/target" "$TMP/alias-into-checkout"
    log=$(new_log)
    out=$log/out
    rc=0
    "${clean_env[@]}" STUB_LOG="$log" CI_HUB_VALIDATE_PRODUCER=systemd-user-v1 \
        TMPDIR="$TMP/alias-into-checkout/tmp" \
        "$hermit/scripts/validate-front-door.sh" -- --all >"$out" 2>&1 || rc=$?
    seen_tmp=$(cat "$log/tmpdir" 2>/dev/null || true)
    local relative_tmp relative_seen rc_rel=0
    relative_tmp=$(new_log)
    (cd "$hermit" && "${clean_env[@]}" STUB_LOG="$relative_tmp" CI_HUB_VALIDATE_PRODUCER=systemd-user-v1 \
        TMPDIR=target/tmp ./scripts/validate-front-door.sh -- --all >"$relative_tmp/out" 2>&1) || rc_rel=$?
    relative_seen=$(cat "$relative_tmp/tmpdir" 2>/dev/null || true)
    if ((rc == 0 && rc_rel == 0)) && [[ $seen_tmp == /tmp/hermit-validate-?????? ]] \
        && [[ $relative_seen == /tmp/hermit-validate-?????? ]]; then
        pass "route: symlinked and relative TMPDIRs that resolve inside the checkout are replaced"
    else
        fail "route: aliased TMPDIRs are replaced" "symlinked: rc=$rc TMPDIR=$seen_tmp" \
            "relative: rc=$rc_rel TMPDIR=$relative_seen"
    fi

    # R3e: with no usable parent for the private TMPDIR, the run is refused by
    # name before the driver starts.
    log=$(new_log)
    out=$log/out
    rc=0
    "${clean_env[@]}" STUB_LOG="$log" CI_HUB_VALIDATE_PRODUCER=systemd-user-v1 \
        HERMIT_VALIDATE_TMPDIR_PARENTS="$TMP/no-such-parent" \
        "$hermit/scripts/validate-front-door.sh" -- --all >"$out" 2>&1 || rc=$?
    if ((rc == 1)) && [[ ! -e $log/driver ]] && grep -q 'no usable parent for a private TMPDIR' "$out"; then
        pass "route: no usable TMPDIR parent is refused by name before the driver"
    else
        fail "route: no usable TMPDIR parent is refused" "rc=$rc" "$(cat "$out")"
    fi

    # Section headers: one per section, each saying what runs and what it gates
    # (R3b's run, with the substituted TMPDIR named in it).
    if grep -Eq '^validate: == scripts/validate.rs --all with TMPDIR=/tmp/hermit-validate-[^ ]+ \(outside the checkout, removed once the driver and everything it started have exited\): .*pre\.submodules.*; gates ' "$header_out" \
        && grep -q '^validate: == front door: inside a ci-hub validate-run unit' "$header_out"; then
        pass "route: each section opens with a header naming what runs and what it gates"
    else
        fail "route: section headers" "$(cat "$header_out")"
    fi

    # R3f: a process the driver started that outlives it keeps the private
    # TMPDIR: the reaper waits for the last holder of the ownership lock, not
    # for the driver, and removes the directory only once that child exits.
    log=$(new_log)
    out=$log/out
    rc=0
    "${clean_env[@]}" STUB_LOG="$log" STUB_SPAWN_CHILD=1 CI_HUB_VALIDATE_PRODUCER=systemd-user-v1 \
        TMPDIR="$hermit/target/tmp" \
        "$hermit/scripts/validate-front-door.sh" -- --all >"$out" 2>&1 || rc=$?
    seen_tmp=$(cat "$log/tmpdir" 2>/dev/null || true)
    local kept_while_child='no'
    if [[ -e $log/child-ready && ! -e $log/child-done ]]; then
        # The driver has exited; give a reaper that wrongly waited only for it
        # time to act before looking.
        sleep 1
        [[ -n $seen_tmp && -d $seen_tmp ]] && kept_while_child='yes'
    fi
    : >"$log/child-release"
    tenth=0
    while [[ ! -e $log/child-done ]] && ((tenth < REAPER_WALL_BACKSTOP_TENTHS)); do
        sleep 0.1
        tenth=$((tenth + 1))
    done
    if ((rc == 0)) && [[ $seen_tmp == /tmp/hermit-validate-?????? && $kept_while_child == yes ]] \
        && [[ -e $log/child-done ]] && wait_gone "$seen_tmp"; then
        pass "route: a child that outlives the driver keeps the private TMPDIR until it exits; then the reaper removes it"
    else
        fail "route: an outliving child keeps the private TMPDIR" "rc=$rc" "TMPDIR $seen_tmp" \
            "kept while the child lived: $kept_while_child" \
            "child finished: $([[ -e $log/child-done ]] && echo yes || echo no)" \
            "still present: $([[ -n $seen_tmp && -e $seen_tmp ]] && echo yes || echo no)" "$(cat "$out")"
    fi

    # R3g: a candidate parent mounted nosuid or nodev is skipped (Hermit's
    # container cannot use it), read through findmnt; a fake findmnt on PATH
    # stands in for such mounts. With every candidate rejected, the run is
    # refused by name before the driver.
    local mounts=$TMP/mounts fake_bin=$TMP/fake-findmnt
    mkdir -p "$mounts/nosuid" "$mounts/nodev" "$mounts/plain" "$fake_bin"
    cat >"$fake_bin/findmnt" <<'FAKE'
#!/usr/bin/env bash
target=
while (($# > 0)); do
    case $1 in
        --target) target=$2; shift 2 ;;
        *) shift ;;
    esac
done
case $target in
    */nosuid) echo rw,nosuid,relatime ;;
    */nodev) echo rw,nodev,relatime ;;
    *) echo rw,relatime ;;
esac
FAKE
    chmod +x "$fake_bin/findmnt"
    log=$(new_log)
    out=$log/out
    rc=0
    "${clean_env[@]}" PATH="$fake_bin:$PATH" STUB_LOG="$log" CI_HUB_VALIDATE_PRODUCER=systemd-user-v1 \
        HERMIT_VALIDATE_TMPDIR_PARENTS="$mounts/nosuid $mounts/nodev $mounts/plain" TMPDIR="$hermit/target/tmp" \
        "$hermit/scripts/validate-front-door.sh" -- --all >"$out" 2>&1 || rc=$?
    seen_tmp=$(cat "$log/tmpdir" 2>/dev/null || true)
    local rejected_log rc_rejected=0
    rejected_log=$(new_log)
    "${clean_env[@]}" PATH="$fake_bin:$PATH" STUB_LOG="$rejected_log" CI_HUB_VALIDATE_PRODUCER=systemd-user-v1 \
        HERMIT_VALIDATE_TMPDIR_PARENTS="$mounts/nosuid $mounts/nodev" TMPDIR="$hermit/target/tmp" \
        "$hermit/scripts/validate-front-door.sh" -- --all >"$rejected_log/out" 2>&1 || rc_rejected=$?
    if ((rc == 0)) && [[ $seen_tmp == "$(realpath -- "$mounts/plain")"/hermit-validate-?????? ]] \
        && wait_gone "$seen_tmp" && ((rc_rejected == 1)) && [[ ! -e $rejected_log/driver ]] \
        && grep -q 'no usable parent for a private TMPDIR' "$rejected_log/out"; then
        pass "route: nosuid and nodev parents are skipped, and with none left the run is refused before the driver"
    else
        fail "route: nosuid/nodev parents are skipped" "rc=$rc TMPDIR=$seen_tmp (want under $mounts/plain)" \
            "all rejected: rc=$rc_rejected, driver ran: $([[ -e $rejected_log/driver ]] && echo yes || echo no)" \
            "$(cat "$out")" "$(cat "$rejected_log/out")"
    fi

    # R4: the driver builds against the reverie submodule, so an unpopulated
    # one is refused by name before the driver starts.
    local bare=$TMP/bare
    plant_checkout "$bare" yes
    # An uninitialized submodule leaves an EMPTY directory, not none.
    rm -r "$bare/hermit/reverie"
    mkdir "$bare/hermit/reverie"
    log=$(new_log)
    out=$log/out
    rc=0
    "${clean_env[@]}" STUB_LOG="$log" CI_HUB_VALIDATE_PRODUCER=systemd-user-v1 \
        "$bare/hermit/scripts/validate-front-door.sh" -- --all >"$out" 2>&1 || rc=$?
    if ((rc == 1)) && [[ ! -e $log/driver ]] && grep -q 'make check-submodules' "$out"; then
        pass "route: an unpopulated reverie is refused by name before the driver"
    else
        fail "route: an unpopulated reverie is refused before the driver" "rc=$rc" "$(cat "$out")"
    fi

    # R4b: a populated reverie checked out at a commit other than the gitlink
    # HEAD records is refused before the driver builds against it; at the
    # recorded commit the driver starts.
    local linked=$TMP/linked rv recorded_rev stale_rev
    plant_checkout "$linked" yes
    rv=$linked/hermit/reverie
    git -C "$linked/hermit" rm -r -q --cached reverie
    git -C "$rv" init -q
    git -C "$rv" add -A
    git -C "$rv" -c user.name=front-door-test -c user.email=test@example.invalid commit -q -m recorded
    recorded_rev=$(git -C "$rv" rev-parse HEAD)
    printf '# moved on\n' >>"$rv/reverie/Cargo.toml"
    git -C "$rv" -c user.name=front-door-test -c user.email=test@example.invalid commit -q -am stale
    stale_rev=$(git -C "$rv" rev-parse HEAD)
    git -C "$linked/hermit" update-index --add --cacheinfo "160000,$recorded_rev,reverie"
    git -C "$linked/hermit" -c user.name=front-door-test -c user.email=test@example.invalid \
        commit -q -m 'record the reverie gitlink'
    log=$(new_log)
    out=$log/out
    rc=0
    "${clean_env[@]}" STUB_LOG="$log" CI_HUB_VALIDATE_PRODUCER=systemd-user-v1 \
        "$linked/hermit/scripts/validate-front-door.sh" -- --all >"$out" 2>&1 || rc=$?
    if ((rc == 1)) && [[ ! -e $log/driver ]] \
        && grep -Fq "is at $stale_rev but HEAD records $recorded_rev" "$out"; then
        pass "route: a reverie at a commit other than its gitlink is refused by name before the driver"
    else
        fail "route: a stale reverie is refused before the driver" "rc=$rc" "$(cat "$out")"
    fi
    git -C "$rv" checkout -q "$recorded_rev"
    log=$(new_log)
    out=$log/out
    rc=0
    "${clean_env[@]}" STUB_LOG="$log" CI_HUB_VALIDATE_PRODUCER=systemd-user-v1 \
        "$linked/hermit/scripts/validate-front-door.sh" -- --all >"$out" 2>&1 || rc=$?
    if ((rc == 0)) && [[ -e $log/driver ]]; then
        pass "route: a reverie at its recorded gitlink starts the driver"
    else
        fail "route: a reverie at its recorded gitlink starts the driver" "rc=$rc" "$(cat "$out")"
    fi

    # R5: outside a dev-hermit parent there is no ci-hub to route to.
    local solo=$TMP/solo
    plant_checkout "$solo" no
    log=$(new_log)
    out=$log/out
    rc=0
    "${clean_env[@]}" STUB_LOG="$log" CLAUDECODE=1 \
        "$solo/hermit/scripts/validate-front-door.sh" -- --all >"$out" 2>&1 || rc=$?
    if ((rc == 0)) && [[ $(cat "$log/driver" 2>/dev/null) == "--all" ]] \
        && grep -q 'no dev-hermit parent' "$out"; then
        pass "route: a standalone checkout runs validate without classifying the caller"
    else
        fail "route: a standalone checkout passes through" "rc=$rc" "$(cat "$out")"
    fi

    # R6-R8: the human route, detached from this process tree.
    if ! command -v setsid >/dev/null 2>&1; then
        skip "route: the human route" "setsid is not installed, so no caller can be detached"
        return 0
    fi
    log=$(new_log)
    rc=$(run_detached "$log" "$log/classify" -- bash "$FRONT_DOOR" --classify)
    if [[ $rc != 0 ]] || ! grep -q '^human ' "$log/classify"; then
        skip "route: the human route" \
            "a detached cleared-environment child is not classified human here ($(cat "$log/classify" 2>/dev/null)); its nearest subreaper is an agent"
        return 0
    fi

    log=$(new_log)
    rc=$(run_detached "$log" "$log/out" STUB_LOG="$log" PATH="$proxybin:$PATH" -- \
        "$hermit/scripts/validate-front-door.sh" -- --all)
    if [[ $rc == 0 ]] && [[ $(tr '\n' ' ' <"$log/driver" 2>/dev/null) == "--human-off-the-record --all " ]] \
        && grep -q '^validate: OFF THE RECORD: a human run' "$log/out" \
        && grep -q 'not validation evidence' "$log/out" \
        && ! grep -q 'tree is not clean' "$log/out" && [[ ! -e $log/submodules ]] \
        && [[ $(cat "$log/proxy" 2>/dev/null) == with-proxy ]] \
        && grep -q '^validate: == with-proxy scripts/validate.rs --human-off-the-record --all with TMPDIR=' "$log/out" \
        && [[ $(cat "$log/tmpdir" 2>/dev/null) == /tmp/hermit-validate-?????? ]] \
        && [[ -e $log/tmpdir-existed ]] && wait_gone "$(cat "$log/tmpdir")"; then
        pass "route: a human on a clean tree is warned off the record and runs the full validate under with-proxy"
    else
        fail "route: the human route on a clean tree" "rc=$rc" \
            "driver argv: $(tr '\n' ' ' <"$log/driver" 2>/dev/null)" \
            "with-proxy: $(cat "$log/proxy" 2>/dev/null || echo not used)" "$(cat "$log/out" 2>/dev/null)"
    fi

    printf 'changed\n' >>"$hermit/tracked.txt"
    printf 'new\n' >"$hermit/untracked-1.txt"
    printf 'new\n' >"$hermit/untracked-2.txt"
    mkdir -p "$hermit/ignored" "$hermit/ci/validate-ledger"
    printf 'own output\n' >"$hermit/ignored/run.log"
    printf 'own output\n' >"$hermit/ci/validate-ledger/row.jsonl"
    log=$(new_log)
    rc=$(run_detached "$log" "$log/out" STUB_LOG="$log" -- \
        "$hermit/scripts/validate-front-door.sh" -- --all)
    if [[ $rc == 0 ]] && grep -q 'the tree is not clean: 3 changed path(s)' "$log/out" \
        && [[ -e $log/driver ]]; then
        pass "route: a dirty tree is warned with its count (validate's own output excluded) and still runs"
    else
        fail "route: the dirty-tree warning" "rc=$rc" "$(cat "$log/out" 2>/dev/null)"
    fi

    log=$(new_log)
    rc=$(run_detached "$log" "$log/out" STUB_LOG="$log" STUB_DRIVER_EXIT=3 -- \
        "$hermit/scripts/validate-front-door.sh" -- --all)
    if [[ $rc == 3 ]]; then
        pass "route: the driver's exit status is the front door's"
    else
        fail "route: exit status propagation" "rc=$rc (want 3)" "$(cat "$log/out" 2>/dev/null)"
    fi
}

# ------------------------------------------------------------------- e2e

# e2e DRIVER_ARGS...: the real front door and driver on this checkout.
e2e() {
    local log=$TMP/e2e rc
    mkdir -p "$log"
    printf 'e2e: the real front door on %s with driver arguments: %s (detached, cleared)\n' \
        "$ROOT" "$*"
    rc=$(run_detached_unbounded "$log" "$log/out" "$@")
    cat "$log/out"
    if [[ $rc == 0 ]] && grep -q '^validate: OFF THE RECORD: a human run' "$log/out" \
        && grep -Eq '^validate: == (with-proxy )?scripts/validate.rs --human-off-the-record ' "$log/out"; then
        printf 'ok   e2e: the human route ran the real validate off the record\n'
        return 0
    fi
    printf 'FAIL e2e: the human route exited %s\n' "$rc"
    return 1
}

# The e2e case waits for the real driver, which compiles and bounds its own
# DAG, so the wait carries only a generous WALL backstop. That backstop also
# ends the wait if the detached run is killed before it reports, because the
# FIFO is held open read-write here. The run inherits only what the driver needs
# from the caller's environment.
readonly E2E_WALL_BACKSTOP=21600
run_detached_unbounded() {
    local log=$1 out=$2 fifo fd rc line
    shift 2
    fifo=$log/done.fifo
    mkfifo "$fifo"
    exec {fd}<>"$fifo"
    setsid -f env -i "PATH=$PATH" "HOME=${HOME:-/nonexistent}" "LC_ALL=C" \
        "USER=${USER:-}" "TERM=dumb" "ROOT_DIR=$ROOT" "OUT=$out" "FIFO=$fifo" \
        ${CARGO_HOME:+"CARGO_HOME=$CARGO_HOME"} ${RUSTUP_HOME:+"RUSTUP_HOME=$RUSTUP_HOME"} \
        ${XDG_RUNTIME_DIR:+"XDG_RUNTIME_DIR=$XDG_RUNTIME_DIR"} \
        ${DBUS_SESSION_BUS_ADDRESS:+"DBUS_SESSION_BUS_ADDRESS=$DBUS_SESSION_BUS_ADDRESS"} \
        bash -c 'cd "$ROOT_DIR" && ./scripts/validate-front-door.sh -- "$@" >"$OUT" 2>&1; printf "%s\n" "$?" >"$FIFO"' \
        front-door-e2e "$@" 2>/dev/null || true
    if read -r -t "$E2E_WALL_BACKSTOP" line <&"$fd"; then
        rc=$line
    else
        rc="no report within ${E2E_WALL_BACKSTOP} s WALL (the run is still going or died without reporting)"
    fi
    exec {fd}<&-
    printf '%s\n' "$rc"
}

# ---------------------------------------------------------------- driver fetch

# fetch_cases: a standalone checkout whose driver is a rust-script, with a fake
# rust-script (prints a package for --package, else runs the stub driver) and a
# fake cargo (fails `cargo fetch` as FAKE_CARGO_FAIL says) on PATH.
fetch_cases() {
    local fetch=$TMP/fetch fake=$TMP/fetch-bin log out rc mode
    plant_checkout "$fetch" no
    sed -i '1s|.*|#!/usr/bin/env rust-script|' "$fetch/hermit/scripts/validate.rs"
    mkdir -p "$fake" "$TMP/fetch-pkg"
    printf '[package]\nname = "driver"\n' >"$TMP/fetch-pkg/Cargo.toml"
    cat >"$fake/rust-script" <<FAKE
#!/usr/bin/env bash
if [[ \$1 == --package ]]; then printf '%s\n' "$TMP/fetch-pkg"; exit 0; fi
exec bash "\$@"
FAKE
    cat >"$fake/cargo" <<'FAKE'
#!/usr/bin/env bash
[[ $1 == fetch ]] || exit 64
case ${FAKE_CARGO_FAIL:-} in
    ssl) printf 'error: failed to get `dagrun` as a dependency\n  [60] SSL peer certificate or SSH remote key was not OK\n' >&2; exit 101 ;;
    net) printf 'error: failed to get `serde` as a dependency\n  Could not resolve host: index.crates.io\n' >&2; exit 101 ;;
    silent) exit 1 ;;
esac
exit 0
FAKE
    # A fake timeout records how it was called and, for the slow case, reports
    # the bound expired (exit 124) without running cargo.
    cat >"$fake/timeout" <<'FAKE'
#!/usr/bin/env bash
printf '%s\n' "$*" >"$STUB_LOG/timeout-args"
[[ ${FAKE_CARGO_FAIL:-} == slow ]] && exit 124
exec /usr/bin/timeout "$@"
FAKE
    chmod +x "$fake/rust-script" "$fake/cargo" "$fake/timeout"
    for mode in ssl net silent slow ok; do
        log=$(new_log)
        out=$log/out
        rc=0
        "${clean_env[@]}" PATH="$fake:$PATH" STUB_LOG="$log" FAKE_CARGO_FAIL="$mode" \
            "$fetch/hermit/scripts/validate-front-door.sh" -- --all >"$out" 2>&1 || rc=$?
        case $mode in
            ssl)
                if ((rc == 75)) && [[ ! -e $log/driver ]] && grep -q 'Cargo could not fetch' "$out" \
                    && grep -q 'SSL peer certificate' "$out" && grep -q 'CARGO_NET_GIT_FETCH_WITH_CLI=true' "$out"; then
                    pass "fetch: a TLS failure fetching the driver's dependencies refuses at once, naming CARGO_NET_GIT_FETCH_WITH_CLI"
                else
                    fail "fetch: a TLS failure fetching the driver's dependencies" "rc=$rc" "$(cat "$out")"
                fi
                ;;
            net)
                if ((rc == 75)) && [[ ! -e $log/driver ]] && grep -q 'Could not resolve host' "$out" \
                    && grep -q 'give Cargo network access' "$out" && ! grep -q CARGO_NET_GIT_FETCH_WITH_CLI "$out"; then
                    pass "fetch: any other fetch failure refuses at once with Cargo's words and the network remedy"
                else
                    fail "fetch: any other fetch failure" "rc=$rc" "$(cat "$out")"
                fi
                ;;
            silent)
                # A fetch that fails with nothing on stderr: under errexit and
                # pipefail the empty log must not end the script before the
                # remedy and exit 75.
                if ((rc == 75)) && [[ ! -e $log/driver ]] && grep -q 'Cargo could not fetch' "$out" \
                    && grep -q 'the fetch exited 1 with no output on stderr' "$out" \
                    && grep -q 'give Cargo network access' "$out"; then
                    pass "fetch: a fetch that fails silently still refuses at once with the network remedy"
                else
                    fail "fetch: a fetch that fails silently" "rc=$rc" "$(cat "$out")"
                fi
                ;;
            slow)
                if ((rc == 75)) && [[ ! -e $log/driver ]] && grep -q 'did not finish within 300 s (WALL)' "$out" \
                    && [[ $(cat "$log/timeout-args" 2>/dev/null) == "--kill-after=10s 300s cargo fetch --manifest-path "* ]]; then
                    pass "fetch: a fetch past its 300 s WALL bound refuses at once, named as such"
                else
                    fail "fetch: a fetch past its WALL bound" "rc=$rc" "timeout args: $(cat "$log/timeout-args" 2>/dev/null)" "$(cat "$out")"
                fi
                ;;
            ok)
                if ((rc == 0)) && [[ $(cat "$log/driver" 2>/dev/null) == "--all" ]] \
                    && [[ $(cat "$log/timeout-args" 2>/dev/null) == "--kill-after=10s 300s cargo fetch --manifest-path "* ]]; then
                    pass "fetch: fetched dependencies start the driver"
                else
                    fail "fetch: fetched dependencies start the driver" "rc=$rc" "$(cat "$out")"
                fi
                ;;
        esac
    done
}

# ---------------------------------------------------------------- make target

# make_cases: `make validate ARGS=...` (dry run) runs the front door with
# exactly `-- --all-unless-level` and then ARGS, word for word. The level
# decision is the driver's (scripts/validate.rs all_unless_level_tests); the
# Makefile must not make one of its own, by filtering ARGS or adding --all.
make_cases() {
    local args got rc want
    for args in "" --keep-going --quick quick "--quick --keep-going" "--only quick e2e.manifest_compat" \
        "--baseline full" "--write-constructed-dag quick"; do
        rc=0
        # An independent make: under a parent make (`make lint-checks`) the
        # inherited MAKEFLAGS make the child warn about the jobserver on stderr.
        # Only stdout is compared; stderr is shown on a failure.
        got=$(env -u MAKEFLAGS -u MFLAGS -u MAKELEVEL \
            make -n -s --no-print-directory -f "$MAKEFILE" -C "$ROOT" validate ARGS="$args" 2>"$TMP/make-stderr") || rc=$?
        want="./scripts/validate-front-door.sh -- --all-unless-level${args:+ $args}"
        # make leaves a trailing space when ARGS is empty; words are compared.
        if ((rc == 0)) && [[ $(printf '%s' "$got" | tr -s ' ' | sed 's/ $//') == "$want" ]]; then
            pass "make: ARGS='$args' hands the front door --all-unless-level, then ARGS unchanged"
        else
            fail "make: ARGS='$args'" "rc=$rc" "want: $want" "got: $got" "stderr: $(cat "$TMP/make-stderr")"
        fi
    done
}

# Rerun this whole suite with TMPDIR inside a planted dev-hermit parent: the
# environment the driver gives check.lint_checks on a human's local run. It must
# still pass; before the scratch root moved under /tmp, its standalone case was
# refused there (rc 75).
control_cases() {
    [[ -z ${VALIDATE_FRONT_DOOR_TEST_NESTED:-} ]] || return 0
    local planted=$TMP/planted-dev-hermit out rc=0
    mkdir -p "$planted/ci-hub" "$planted/hermit/target/tmp"
    printf '[submodule "hermit"]\n\tpath = hermit\n\turl = https://example.invalid/hermit\n' \
        >"$planted/.gitmodules"
    out=$("${clean_env[@]}" TMPDIR="$planted/hermit/target/tmp" VALIDATE_FRONT_DOOR_TEST_NESTED=1 \
        VALIDATE_FRONT_DOOR_UNDER_TEST="$FRONT_DOOR" bash "${BASH_SOURCE[0]}" 2>&1) || rc=$?
    if ((rc == 0)) && grep -Eq '^[0-9]+ case\(s\), 0 failed, [0-9]+ skipped$' <<<"$out"; then
        pass "control: the suite passes with TMPDIR inside a planted dev-hermit parent"
    else
        fail "control: the suite with TMPDIR inside a planted dev-hermit parent" "rc=$rc" \
            "$(grep -E '^(FAIL|SKIP)|case\(s\)' <<<"$out")"
    fi
}

main() {
    case ${1:-} in
        "") ;;
        --e2e)
            shift
            (($# > 0)) || set -- --quick
            e2e "$@"
            return
            ;;
        -h | --help)
            awk '!body && /^$/ { body = 1; next } body && /^set -euo/ { exit }
                 body { sub(/^# ?/, ""); print }' "${BASH_SOURCE[0]}"
            return 0
            ;;
        *)
            printf 'test-validate-front-door: unknown argument %s (try --help)\n' "$1" >&2
            return 2
            ;;
    esac
    classifier_cases
    cost_cases
    route_cases
    control_cases
    fetch_cases
    make_cases
    printf '%d case(s), %d failed, %d skipped\n' "$cases" "$failures" "$skips"
    ((failures == 0))
}

main "$@"
