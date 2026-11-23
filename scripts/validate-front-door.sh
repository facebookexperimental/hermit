#!/usr/bin/env bash
# Copyright (c) Meta Platforms, Inc. and affiliates.
# All rights reserved.
#
# This source code is licensed under the BSD-style license found in the
# LICENSE file in the root directory of this source tree.

# The front door of `make validate`: one fast routing decision, taken before
# anything is compiled, locked or checked.
#
# Inside the dev-hermit parent checkout, `make validate` used to build
# scripts/validate.rs (a release rust-script build; ~35 s to ~4 min after a
# pull) and only then tell an unadmitted caller no. This script decides first,
# in milliseconds, from /proc and the environment:
#
#   agent   an agent harness (claude, muse, codex, agentctl) is among this
#           process's ancestors, an agent environment marker is set, or the
#           caller's herdr pane is an agent pane. Refused in at most three
#           lines naming the exact `ci-hub validate-run` command: agents
#           validate through ci-hub, which holds the lock and publishes.
#   human   none of those. Warned that the run is OFF THE RECORD (no ledger
#           row, no receipt, not validation evidence) and, when the tree is
#           dirty, by how many paths; then the FULL validate runs in place
#           with --human-off-the-record.
#
# Two routes skip the classification and run the driver with the caller's
# arguments: a run inside a ci-hub validate-run unit (CI_HUB_VALIDATE_PRODUCER
# is set; the driver's own lock admission decides), and any checkout outside a
# dev-hermit parent (no ci-hub to route to). Every route starts the driver
# directly: its DAG's first node, pre.submodules, verifies the submodule pins,
# so no copy runs in front of it. Every route outside a ci-hub unit, the human
# route and a checkout outside a dev-hermit parent alike, starts the driver
# under with-proxy when with-proxy exists, as the unit itself already runs
# under it (see run_validate); each route's section header shows it.
#
# THIS IS ROUTING, NOT A SECURITY BOUNDARY. A caller can clear its environment
# and detach from its ancestors; nothing here is meant to stop that. An agent
# looks human, for example, when it runs `make validate` through a
# `systemd-run --user` service, through `setsid --fork` under `env -i`, over ssh
# to a peer host, or in a window of a tmux server started elsewhere. herdr-run
# refuses `make` by its allowlist policy. A human typing into an agent session
# (Claude Code's `!` prefix) is routed as an agent. What the classification buys
# is that an agent following its instructions gets the right answer at once,
# and a human at a terminal gets a run instead of a refusal. Evidence integrity
# does not depend on it: only ci-hub's lock admission can make a run publish,
# and an off-the-record run never does, whoever starts it.
#
# Usage:
#   scripts/validate-front-door.sh [--] [DRIVER_ARGS...]
#   scripts/validate-front-door.sh --classify [--proc-root DIR] [--start-pid PID] [--herdr PATH]
#   scripts/validate-front-door.sh --help
#
# Exit status: the driver's own status on every route that runs it; 75
# (EX_TEMPFAIL, the driver's "could not run") for the agent refusal; 2 for a
# usage error. --classify exits 0 and prints "agent <reason>" or
# "human <reason>" on one line.

set -euo pipefail

readonly AGENT_REFUSED_EXIT=75
# The WALL backstop of the front door's own fetch of the driver's Cargo
# dependencies (fetch_driver_dependencies). Cargo's network timeouts bound each
# request, but nothing else bounds the whole fetch. On the 2026-11-22 fresh
# clones the whole cold driver build, its fetch included, ended within 72-160 s;
# 300 s is a generous backstop, not an estimate.
readonly DRIVER_FETCH_WALL_SECONDS=300

# Executables whose presence among the ancestors makes the caller an agent,
# matched against /proc/PID/comm and the basenames of argv[0] and argv[1]
# (argv[1] catches interpreter launches such as `node .../claude`).
readonly AGENT_PROCESS_NAMES=(claude muse codex agentctl)

# Environment variables that agent harnesses set for the commands they run.
# A non-empty value marks the caller as an agent. Observed 2026-11-21 (host in
# docs/TESTING_ENVIRONMENTS.md, "Named measurement hosts"): Claude Code sets CLAUDECODE, AI_AGENT, AGENT and
# CODING_AGENT_METADATA; Muse sets AGENT, CODING_AGENT_METADATA,
# META_3PAI_AGENT_PLATFORM and MUSE_SESSION_ID; dev-hermit lanes carry
# DG_AGENT_NAME or AGENTCTL_AGENT. Codex commands carry CODEX_THREAD_ID or
# CODEX_SANDBOX, and every Codex launch is also caught by its ancestor.
readonly AGENT_ENV_MARKERS=(
    CLAUDECODE
    AI_AGENT
    AGENT
    CODING_AGENT_METADATA
    META_3PAI_AGENT_PLATFORM
    MUSE_SESSION_ID
    CODEX_THREAD_ID
    CODEX_SANDBOX
    DG_AGENT_NAME
    AGENTCTL_AGENT
)

# The help text is the comment block between the license and `set -euo`.
usage() {
    awk '!body && /^$/ { body = 1; next } body && /^set -euo/ { exit }
         body { sub(/^# ?/, ""); print }' "${BASH_SOURCE[0]}"
}

die_usage() {
    printf 'validate-front-door: %s\n' "$1" >&2
    printf 'validate-front-door: run with --help for usage\n' >&2
    exit 2
}

# Print the name of the first agent environment marker that is set.
agent_env_marker() {
    local name
    for name in "${AGENT_ENV_MARKERS[@]}"; do
        if [[ -n ${!name:-} ]]; then
            printf '%s\n' "$name"
            return 0
        fi
    done
    return 1
}

is_agent_process_name() {
    local candidate=${1##*/} name
    for name in "${AGENT_PROCESS_NAMES[@]}"; do
        [[ $candidate == "$name" ]] && return 0
    done
    return 1
}

# Walk PPid links from START_PID to PID 1 under PROC_ROOT. Print
# "PID NAME" for the first agent ancestor (START_PID itself included).
# Uses only bash builtins: no process is spawned per ancestor.
agent_ancestor() {
    local proc_root=$1 pid=$2 depth=0 comm key value ppid
    local -a argv
    while [[ $pid =~ ^[0-9]+$ ]] && ((pid > 1 && depth < 512)); do
        depth=$((depth + 1))
        comm=
        [[ -r $proc_root/$pid/comm ]] && read -r comm <"$proc_root/$pid/comm" || true
        if [[ -n $comm ]] && is_agent_process_name "$comm"; then
            printf '%s %s\n' "$pid" "$comm"
            return 0
        fi
        argv=()
        if [[ -r $proc_root/$pid/cmdline ]]; then
            mapfile -d '' -t -n 2 argv <"$proc_root/$pid/cmdline" || true
        fi
        if ((${#argv[@]} > 0)) && is_agent_process_name "${argv[0]}"; then
            printf '%s %s\n' "$pid" "${argv[0]##*/}"
            return 0
        fi
        if ((${#argv[@]} > 1)) && is_agent_process_name "${argv[1]}"; then
            printf '%s %s\n' "$pid" "${argv[1]##*/}"
            return 0
        fi
        ppid=
        if [[ -r $proc_root/$pid/status ]]; then
            while read -r key value; do
                if [[ $key == PPid: ]]; then
                    ppid=$value
                    break
                fi
            done <"$proc_root/$pid/status"
        fi
        [[ -n $ppid && $ppid != "$pid" ]] || return 1
        pid=$ppid
    done
    return 1
}

# When the caller runs in a herdr pane, ask herdr whether that pane hosts an
# agent. Print the agent kind and succeed only for an agent pane. An
# unreachable herdr is not evidence either way, so it does not classify.
herdr_agent_pane() {
    local herdr=$1 pane=${HERDR_PANE_ID:-} reply
    [[ -n $pane ]] || return 1
    [[ -n $herdr && -x $herdr ]] || return 1
    reply=$(timeout 1 "$herdr" pane get "$pane" 2>/dev/null) || return 1
    if [[ $reply =~ \"agent\":\"([^\"]+)\" ]]; then
        printf '%s\n' "${BASH_REMATCH[1]}"
        return 0
    fi
    return 1
}

default_herdr() {
    if [[ -n ${HERDR_BIN_PATH:-} ]]; then
        printf '%s\n' "$HERDR_BIN_PATH"
    else
        command -v herdr 2>/dev/null || true
    fi
}

# classify PROC_ROOT START_PID HERDR -> prints "agent <reason>" or "human <reason>"
classify() {
    local proc_root=$1 start_pid=$2 herdr=$3 found kind
    if found=$(agent_env_marker); then
        printf 'agent environment marker %s is set\n' "$found"
        return 0
    fi
    if found=$(agent_ancestor "$proc_root" "$start_pid"); then
        printf 'agent ancestor process %s is %s\n' "${found%% *}" "${found#* }"
        return 0
    fi
    if kind=$(herdr_agent_pane "$herdr"); then
        printf 'agent herdr pane %s hosts a %s agent\n' "$HERDR_PANE_ID" "$kind"
        return 0
    fi
    printf 'human no agent ancestor, agent environment marker or herdr agent pane\n'
}

# The dev-hermit parent: the nearest ancestor directory whose .gitmodules
# declares the hermit submodule at path "hermit" (scripts/validate.rs
# find_parent), or DEV_HERMIT_PARENT when the caller supplies one.
find_parent() {
    local dir=$1
    if [[ -n ${DEV_HERMIT_PARENT:-} ]]; then
        printf '%s\n' "$DEV_HERMIT_PARENT"
        return 0
    fi
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

shell_quote_args() {
    local out='' arg
    for arg in "$@"; do
        out+=" $(printf '%q' "$arg")"
    done
    printf '%s\n' "${out# }"
}

# Count `git status --porcelain` paths, excluding validate's own output
# (ci/validate-ledger and ignored/, as scripts/validate.rs SELF_OUTPUT_PREFIXES).
dirty_path_count() {
    local root=$1 line path count=0
    while IFS= read -r line; do
        path=${line:3}
        case $path in
            ci/validate-ledger | ci/validate-ledger/* | ignored | ignored/*) continue ;;
        esac
        count=$((count + 1))
    done < <(GIT_OPTIONAL_LOCKS=0 git -C "$root" status --porcelain 2>/dev/null)
    printf '%s\n' "$count"
}

section() {
    printf 'validate: == %s\n' "$1" >&2
}

# The driver with DRIVER_ARGS, behind its section header.
#
# No pre-gate runs in sequence here. verify-submodules is the DAG's first node,
# pre.submodules, which every node that reads a submodule depends on; running
# it again in front of the driver only added its time to every run. What the
# driver BUILD itself needs is narrower: its path dependencies reach the Reverie
# crates in the reverie submodule. So two cases are refused here by name,
# instead of surfacing as a Cargo error or as a driver built against the wrong
# Reverie: an unpopulated reverie (an uninitialized submodule is an empty
# directory), and a populated one checked out at a commit other than the
# gitlink HEAD records (two git calls). A dirty reverie and every other
# submodule stay pre.submodules' job; nothing before that node tests or
# publishes anything.
#
# TMPDIR: the driver points an unset TMPDIR at target/validation/run-*/tmp,
# inside this Cargo workspace, and then its own self-test node refuses ("run the
# self-test with TMPDIR outside every Cargo workspace"); a quick run from a
# shell without TMPDIR failed that way on 2026-11-21. ci-hub's validate-run
# always supplies one (ci-hub/validate/start_unit.py runtime_root_parent: the
# first of /tmp, /var/tmp and /run/user/UID mounted without nosuid or nodev,
# which Hermit's container needs). So a run without one, or with one that
# resolves inside this checkout, gets a private hermit-validate-XXXXXX under the
# same first usable parent.
#
# The driver is still exec'd, so no front-door process is left between the
# caller and the driver: a signal to the process `make` started reaches the
# driver itself. The private TMPDIR is removed by a detached reaper once the
# driver and every process that inherited its ownership lock (fd 9) have
# exited; a process that keeps the lock alive keeps the directory, never the
# reverse.
#
# Driver dependencies: before the driver is exec'd, Cargo fetches its
# dependencies (fetch_driver_dependencies), so a fetch failure stops the run at
# once with a named reason and remedy instead of ending a build in a raw Cargo
# error. On a host behind a TLS-intercepting proxy, Cargo's built-in Git cannot
# fetch a git dependency (agent-utils) without net.git-fetch-with-cli; measured
# on a fresh clone with an empty Cargo home on 2026-11-22.
#
# with-proxy: ci-hub runs the driver under with-proxy, because nodes such as
# setup.pinned_root_fetch fetch from crates.io and GitHub, and a host may have
# no direct egress. Every route outside a ci-hub unit, which already carries
# it, does the same when with-proxy exists.
# fetch_driver_dependencies LAUNCHER...: when the driver is a rust-script,
# fetch the dependencies of the package rust-script generates for it, with the
# same launcher the driver will run under. A failure exits 75 (could not run)
# naming Cargo's own last lines and the remedy; anything else that keeps the
# package from being generated is left to the driver's build to report.
fetch_driver_dependencies() {
    local -a launcher=("$@")
    local first pkg log
    first=$(head -n 1 scripts/validate.rs 2>/dev/null || true)
    [[ $first == '#!'*rust-script* ]] || return 0
    command -v rust-script >/dev/null 2>&1 && command -v cargo >/dev/null 2>&1 || return 0
    pkg=$(rust-script --package ./scripts/validate.rs 2>/dev/null) || return 0
    [[ -f $pkg/Cargo.toml ]] || return 0
    log=$(mktemp "${TMPDIR:-/tmp}/validate-driver-fetch.XXXXXX")
    local status=0
    "${launcher[@]}" timeout --kill-after=10s "${DRIVER_FETCH_WALL_SECONDS}s" \
        cargo fetch --manifest-path "$pkg/Cargo.toml" >/dev/null 2>"$log" || status=$?
    if ((status == 0)); then
        rm -f -- "$log"
        return 0
    fi
    if ((status == 124 || status == 137)); then
        printf 'validate: REFUSED: Cargo'"'"'s fetch of the validation driver'"'"'s dependencies did not finish within %s s (WALL), so the driver cannot be built; nothing has run\n' \
            "$DRIVER_FETCH_WALL_SECONDS" >&2
        printf 'validate: remedy: check this host'"'"'s network path to crates.io and GitHub (a proxy goes in ~/.cargo/config.toml [http] proxy or https_proxy), then retry\n' >&2
        rm -f -- "$log"
        exit 75
    fi
    printf 'validate: REFUSED: Cargo could not fetch the validation driver'"'"'s dependencies, so the driver cannot be built; nothing has run. Cargo said:\n' >&2
    # Under errexit and pipefail, grep's status 1 for a log with no nonblank
    # line would end the script here, before the remedy and exit 75; a fetch
    # that fails silently is reported as such instead.
    local said
    said=$(grep -v '^[[:space:]]*$' "$log" | tail -n 6) || true
    if [[ -n $said ]]; then
        printf '%s\n' "$said" | sed 's/^/    /' >&2
    else
        printf '    (nothing: the fetch exited %s with no output on stderr)\n' "$status" >&2
    fi
    if grep -qiE 'ssl|certificate' "$log"; then
        printf 'validate: remedy: behind a TLS-intercepting proxy, Cargo'"'"'s built-in Git does not trust the proxy'"'"'s certificate; set CARGO_NET_GIT_FETCH_WITH_CLI=true (or net.git-fetch-with-cli = true in ~/.cargo/config.toml) so Cargo fetches with git, then retry\n' >&2
    else
        printf 'validate: remedy: give Cargo network access to crates.io and GitHub (a proxy goes in ~/.cargo/config.toml [http] proxy or https_proxy), then retry\n' >&2
    fi
    rm -f -- "$log"
    exit 75
}

run_validate() {
    local root=$1
    shift
    cd "$root"
    if [[ ! -f reverie/reverie/Cargo.toml ]]; then
        printf 'validate: the reverie submodule is not populated (no reverie/reverie/Cargo.toml), and the driver builds against it; run `make check-submodules`, then retry\n' >&2
        exit 1
    fi
    local recorded checked_out
    recorded=$(GIT_OPTIONAL_LOCKS=0 git ls-tree HEAD reverie 2>/dev/null || true)
    if [[ $recorded == "160000 commit "* && -e reverie/.git ]]; then
        recorded=${recorded#160000 commit }
        recorded=${recorded%%$'\t'*}
        checked_out=$(GIT_OPTIONAL_LOCKS=0 git -C reverie rev-parse HEAD 2>/dev/null || true)
        if [[ $checked_out != "$recorded" ]]; then
            printf 'validate: the reverie submodule is at %s but HEAD records %s, and the driver builds against it; run `make check-submodules`, then retry\n' \
                "${checked_out:-an unreadable commit}" "$recorded" >&2
            exit 1
        fi
    fi
    local -a launcher=()
    local shown=''
    if [[ -z ${CI_HUB_VALIDATE_PRODUCER:-} ]] && command -v with-proxy >/dev/null 2>&1; then
        launcher=(with-proxy)
        shown='with-proxy '
    fi
    local caller_tmp='' real_root
    real_root=$(realpath -- "$root")
    [[ -z ${TMPDIR:-} ]] || caller_tmp=$(realpath -m -- "$TMPDIR")
    if [[ -n $caller_tmp && $caller_tmp != "$real_root" && $caller_tmp != "$real_root"/* ]]; then
        section "${shown}scripts/validate.rs $(shell_quote_args "$@"): rebuilds the driver if its sources changed (release build: 35 s for the driver alone, ~80 s after a typical pull), then runs the selected profile, whose first node pre.submodules verifies every submodule pin; gates the verdict"
        fetch_driver_dependencies "${launcher[@]}"
        exec "${launcher[@]}" ./scripts/validate.rs "$@"
    fi
    local tmp_parent run_tmp
    if ! tmp_parent=$(private_tmpdir_parent); then
        printf 'validate: no usable parent for a private TMPDIR: every candidate (%s) is missing or mounted nosuid or nodev, which Hermit'"'"'s container cannot use; set TMPDIR to a directory outside this checkout on a filesystem without them\n' \
            "$(tmpdir_parent_candidates | tr '\n' ' ' | sed 's/ $//')" >&2
        exit 1
    fi
    run_tmp=$(mktemp -d "$tmp_parent/hermit-validate-XXXXXX")
    # The ownership lock: fd 9 is inherited by the driver and by every process
    # it starts. The reaper's own open of the file blocks until the last of
    # them closes fd 9, then removes the directory (and only a path of exactly
    # this shape).
    exec 9>"$run_tmp/.owner"
    flock -x 9
    setsid -f bash -c 'flock -x "$1" true; case $2 in */hermit-validate-??????) rm -rf -- "$2" ;; esac' \
        hermit-validate-tmpdir-reaper "$run_tmp/.owner" "$run_tmp" 9>&- </dev/null >/dev/null 2>&1
    export TMPDIR=$run_tmp
    section "${shown}scripts/validate.rs $(shell_quote_args "$@") with TMPDIR=$run_tmp (outside the checkout, removed once the driver and everything it started have exited): rebuilds the driver if its sources changed (release build: 35 s for the driver alone, ~80 s after a typical pull), then runs the selected profile, whose first node pre.submodules verifies every submodule pin; gates the verdict"
    fetch_driver_dependencies "${launcher[@]}"
    exec "${launcher[@]}" ./scripts/validate.rs "$@"
}

# The parents ci-hub's start_unit.py runtime_root_parent tries, in order;
# HERMIT_VALIDATE_TMPDIR_PARENTS (space-separated) replaces the list.
tmpdir_parent_candidates() {
    local -a configured=()
    if [[ -n ${HERMIT_VALIDATE_TMPDIR_PARENTS:-} ]]; then
        read -r -a configured <<<"$HERMIT_VALIDATE_TMPDIR_PARENTS"
        printf '%s\n' "${configured[@]}"
    else
        printf '%s\n' /tmp /var/tmp "/run/user/$(id -u)"
    fi
}

# The first candidate that is a directory on a filesystem mounted without
# nosuid and nodev (findmnt). A host without findmnt cannot tell, so its first
# existing candidate is used.
private_tmpdir_parent() {
    local candidate options
    while IFS= read -r candidate; do
        [[ -d $candidate ]] || continue
        if command -v findmnt >/dev/null 2>&1; then
            options=$(findmnt -n -o OPTIONS --target "$candidate" 2>/dev/null) || continue
            [[ ,$options, == *,nosuid,* || ,$options, == *,nodev,* ]] && continue
        fi
        realpath -- "$candidate"
        return 0
    done < <(tmpdir_parent_candidates)
    return 1
}

main() {
    local mode=run proc_root=/proc start_pid=$$ herdr root parent decision
    herdr=$(default_herdr)
    while (($# > 0)); do
        case $1 in
            -h | --help)
                usage
                return 0
                ;;
            --classify)
                mode=classify
                shift
                ;;
            --proc-root | --start-pid | --herdr)
                [[ $mode == classify ]] || die_usage "$1 is accepted only with --classify"
                (($# >= 2)) || die_usage "$1 needs a value"
                case $1 in
                    --proc-root) proc_root=$2 ;;
                    --start-pid) start_pid=$2 ;;
                    --herdr) herdr=$2 ;;
                esac
                shift 2
                ;;
            --)
                shift
                break
                ;;
            *)
                [[ $mode == run ]] || die_usage "unexpected argument with --classify: $1"
                break
                ;;
        esac
    done

    if [[ $mode == classify ]]; then
        (($# == 0)) || die_usage "--classify takes no driver arguments"
        classify "$proc_root" "$start_pid" "$herdr"
        return 0
    fi

    root=$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd -P)
    if ! parent=$(find_parent "${root%/*}") || [[ ! -d $parent/ci-hub ]]; then
        section "front door: no dev-hermit parent, so nothing to route; running validate without classifying the caller"
        run_validate "$root" "$@"
    fi
    if [[ -n ${CI_HUB_VALIDATE_PRODUCER:-} ]]; then
        section "front door: inside a ci-hub validate-run unit; the driver's lock admission decides"
        run_validate "$root" "$@"
    fi

    decision=$(classify "$proc_root" "$start_pid" "$herdr")
    if [[ $decision == agent\ * ]]; then
        local commit agent driver_args=''
        commit=$(GIT_OPTIONAL_LOCKS=0 git -C "$root" rev-parse HEAD 2>/dev/null || printf '<commit>')
        agent=${DG_AGENT_NAME:-${AGENTCTL_AGENT:-}}
        if [[ -n $agent ]]; then
            agent=$(printf '%q' "$agent")
        else
            agent="'<registered-agent-name>'"
        fi
        (($# == 0)) || driver_args=" -- $(shell_quote_args "$@")"
        printf 'validate: REFUSED for an agent caller (%s); agents validate through ci-hub, never in place:\n' \
            "${decision#agent }" >&2
        printf '  %q validate-run --checkout %q --agent %s --target %s%s\n' \
            "$parent/ci-hub/ci-hub" "$root" "$agent" "$commit" "$driver_args" >&2
        printf 'validate: routing, not a security boundary: scripts/validate-front-door.sh --help\n' >&2
        return "$AGENT_REFUSED_EXIT"
    fi

    local dirty
    printf 'validate: OFF THE RECORD: a human run (%s); this result is not validation evidence: no ledger row, no receipt\n' \
        "${decision#human }" >&2
    dirty=$(dirty_path_count "$root")
    if ((dirty > 0)); then
        printf 'validate: WARNING: the tree is not clean: %s changed path(s) in `git status --porcelain`; the run tests them, but no commit holds this tree\n' \
            "$dirty" >&2
    fi
    run_validate "$root" --human-off-the-record "$@"
}

main "$@"
