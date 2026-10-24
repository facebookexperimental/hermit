#!/usr/bin/env bash
# Delegate a cgroup v2 subtree to the GitHub-hosted runner account and move one
# process into it, so every descendant of that process can create child cgroups.
#
# Usage: ci/delegate-hosted-cgroup.sh <pid>
#
# Why: every budgeted Nextest attempt runs through nextest-cpu-wrapper, which
# creates a fresh attempt cgroup below the caller's current cgroup, enrolls the
# test there, and measures its CPU from that cgroup's cpu.stat. Local validation
# hosts run inside a user-owned (delegated) systemd scope, so that works there.
# A GitHub-hosted job instead runs in
# /sys/fs/cgroup/system.slice/hosted-compute-agent.service, which root owns and
# has not delegated. Run https://github.com/rrnewton/hermit/actions/runs/36485831200
# failed every attempt of test.detcore_unit_on_host (810 of 810),
# test.detcore_misc_on_host, test.detcore_parallel_on_host and the real Nextest
# fixture in check.lint_checks with
#   nextest-cpu-wrapper: cannot create fresh attempt cgroup
#   /sys/fs/cgroup/system.slice/hosted-compute-agent.service/hermit-nextest-attempt-...:
#   Permission denied (os error 13)
# before any test body executed.
#
# What: the hosted runner account has passwordless sudo. Create one child of
# the process's current cgroup, give the runner account the directory and the
# three files the kernel's cgroup v2 delegation model hands to a delegatee
# (cgroup.procs, cgroup.threads, cgroup.subtree_control), then move the process
# into it. Descendants inherit that cgroup, including across the user namespace
# ci/run-hosted-node.sh enters: the runner uid is mapped to root there, so the
# delegated files stay writable. Moving a test into its attempt cgroup needs
# write access to the common ancestor's cgroup.procs, which is the delegated
# directory's own file.
#
# This enables no controller and sets no limit. The delegated cgroup stays below
# the job's original cgroup, so the runner service's own accounting and limits
# still cover the job. The CPU budget itself is unchanged; this only provides
# the delegated subtree the wrapper already requires.

set -euo pipefail

readonly CGROUP_ROOT=/sys/fs/cgroup
readonly DELEGATED_NAME=hermit-hosted-delegated

die() {
    echo "delegate-hosted-cgroup.sh: $*" >&2
    exit 1
}

if [[ ${GITHUB_ACTIONS:-} != true || ${RUNNER_ENVIRONMENT:-} != github-hosted ]]; then
    echo "delegate-hosted-cgroup.sh: this helper is only for GitHub-hosted runners (GITHUB_ACTIONS=true, RUNNER_ENVIRONMENT=github-hosted)" >&2
    exit 2
fi
if (($# != 1)) || [[ ! $1 =~ ^[1-9][0-9]*$ ]]; then
    echo "usage: ci/delegate-hosted-cgroup.sh <pid>" >&2
    exit 2
fi
pid=$1
[[ -r /proc/$pid/cgroup ]] || die "process $pid does not exist"

fs_type=$(stat -f -c %T "$CGROUP_ROOT") || die "cannot inspect $CGROUP_ROOT"
[[ $fs_type == cgroup2fs ]] ||
    die "$CGROUP_ROOT is $fs_type, not cgroup v2; nextest-cpu-wrapper requires cgroup v2"

unified_cgroup() {
    local lines
    lines=$(sed -n 's/^0:://p' "/proc/$1/cgroup") || return 1
    [[ -n $lines && $lines != *$'\n'* && $lines == /* ]] || return 1
    printf '%s\n' "$lines"
}

current=$(unified_cgroup "$pid") ||
    die "/proc/$pid/cgroup has no single unified cgroup v2 entry"
current_dir=$CGROUP_ROOT${current%/}

# nextest-cpu-wrapper needs exactly these two permissions in its current cgroup:
# mkdir for the attempt cgroup, and cgroup.procs as the common ancestor of the
# move. A runner image that already delegates the job's cgroup needs nothing.
if [[ -w $current_dir && -w $current_dir/cgroup.procs ]]; then
    echo "HOSTED-CGROUP: $current is already delegated to $(id -un); no change" >&2
    exit 0
fi

delegated_dir=$current_dir/$DELEGATED_NAME
sudo mkdir -p -- "$delegated_dir"
sudo chown -- "$(id -u):$(id -g)" "$delegated_dir" \
    "$delegated_dir/cgroup.procs" \
    "$delegated_dir/cgroup.threads" \
    "$delegated_dir/cgroup.subtree_control"
# The source and destination share the root-owned current cgroup as their
# common ancestor, so only root may perform this one move.
sudo sh -c 'printf "%s\n" "$1" > "$2"' sh "$pid" "$delegated_dir/cgroup.procs"

moved=$(unified_cgroup "$pid") ||
    die "/proc/$pid/cgroup has no single unified cgroup v2 entry after the move"
[[ $moved == "${current%/}/$DELEGATED_NAME" ]] ||
    die "process $pid is in $moved after the move, expected ${current%/}/$DELEGATED_NAME"
[[ -w $delegated_dir && -w $delegated_dir/cgroup.procs ]] ||
    die "$delegated_dir is not writable by $(id -un) after delegation"
echo "HOSTED-CGROUP: moved process $pid from $current to delegated $moved (owner $(id -un))" >&2
