#!/usr/bin/env bash
# Copyright (c) Meta Platforms, Inc. and affiliates.
# All rights reserved.
#
# This source code is licensed under the BSD-style license found in the
# LICENSE file in the root directory of this source tree.
#
# Defends the frozen /etc/group control in hermit-cli/src/bin/hermit/container.rs.
#
# WHAT THE CONTROL DOES. `frozen_group_file()` reads the host /etc/group and, if
# no group carries the kernel overflow GID (65534), APPENDS `nobody:x:65534:`
# before bind-mounting the result read-only over /etc/group. Inside the guest's
# user namespace every unmapped group id collapses to that overflow GID, so
# whether it resolves to a NAME or prints as a NUMBER is decided entirely by
# whether that entry exists. Without it, `ls -l`, `id` and `stat -c %G` print
# `65534` on every host whose /etc/group lacks the group, which silently changes
# the stdout of any other cell that lists a file.
#
# ⚠️ THE ASSERTION BELOW IS THE DEFENCE, NOT THE TWO-RUN COMPARISON.
# This control's whole job is to make runs agree, so comparing two runs cannot
# detect its absence: both runs would print the same unresolved value and
# compare equal. A cell that leaned on the comparison would be green whether or
# not the control existed. The guest therefore states what must be true and
# exits nonzero by name when it is not.
#
# BOTH ARMS OBSERVED, not argued:
#   positive  under hermit                        -> `nobody`,  exit 0
#   negative  `unshare --user` (no group mount)   -> `UNKNOWN`, exit 1
# The negative arm is what a missing control actually looks like, and running it
# is what caught the first version of this assertion: it only rejected the
# NUMERIC gid, while coreutils prints `UNKNOWN`, so it passed the negative
# control and would have shipped unable to detect the absence it exists for.
set -euo pipefail

# The kernel's own overflow GID, so this does not hard-code a host assumption.
overflow_gid=$(cat /proc/sys/kernel/overflowgid)

# A file the guest did not create, whose group is unmapped in the user namespace
# and therefore collapses to the overflow GID.
resolved=$(stat -c '%G' /etc/hostname)
printf 'OVERFLOW_GID_GROUP=%s\n' "$resolved"

# The contract is "resolves to a NAME". Two distinct ways it can fail, and the
# first was found only by RUNNING the negative control: GNU coreutils prints
# `UNKNOWN` when getgrgid() finds nothing, NOT the numeric id. An assertion that
# only rejected the number passed the negative control and would have shipped
# unable to detect the very absence it exists for.
case "$resolved" in
    "" )
        printf 'FAIL: stat returned no group for overflow GID %s\n' "$overflow_gid" >&2
        exit 1 ;;
    UNKNOWN )
        printf 'FAIL: overflow GID %s did not resolve; /etc/group has no entry for it, so the frozen group database is missing or lacks the synthesised entry\n' \
            "$overflow_gid" >&2
        exit 1 ;;
    "$overflow_gid" )
        printf 'FAIL: overflow GID %s printed as a number rather than a name\n' "$overflow_gid" >&2
        exit 1 ;;
esac
printf 'OK: overflow GID %s resolves to the name %s\n' "$overflow_gid" "$resolved"
