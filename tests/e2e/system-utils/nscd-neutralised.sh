#!/usr/bin/env bash
# Copyright (c) Meta Platforms, Inc. and affiliates.
# All rights reserved.
#
# This source code is licensed under the BSD-style license found in the
# LICENSE file in the root directory of this source tree.
#
# Defends the empty-directory mount hermit places over /var/run/nscd
# (hermit-cli/src/bin/hermit/container.rs, identity_hardening_mounts). Without
# it the guest's glibc reaches the host nscd socket, and its answers depend on
# the host cache's readiness -- external state hermit does not control.
#
# WHY THIS CELL ASSERTS IN THE GUEST RATHER THAN RELYING ON THE TWO-RUN
# COMPARISON. This is an ordinary verify cell; every verify cell runs a guest
# program, and that program can assert whatever it likes -- an assertion that
# fails exits nonzero and reds the cell. There is no separate kind of cell here.
# What differs is WHICH SIGNAL does the detecting.
#
# The two-run comparison cannot detect this control, and the reason is
# structural: the comparison asks whether the two runs AGREE, and this control's
# whole job is to make them agree. It succeeds identically whether or not the
# control exists. Measured 2026-08-25 on a dedicated host: three back-to-back native
# lookups produced BYTE-IDENTICAL nscd interaction (one connect() to the socket
# each, zero /etc/passwd opens, pid prefixes stripped). The only channel that
# could make the pair disagree is a cache entry expiring between the two runs,
# and positive-time-to-live is 600s -- roughly 0.3% for a two-second pair.
#
# So the comparison is left to do its normal job, and the detection of THIS
# control is done by the assertions below, which fail by name the moment the
# mount is gone.
#
# DEMONSTRATED ABLE TO FAIL, both arms run:
#   under hermit (control present): "OK", exit 0
#   natively     (control removed): "FAIL: host nscd socket is visible", exit 1
#
# TWO LIMITS, STATED HERE RATHER THAN DISCOVERED LATER:
#  1. COLD CACHE IS UNTESTED, NOT DISPROVEN. The cold-miss-then-warm-hit path
#     could in principle diverge within a pair. It could not be measured: `nscd
#     -g` refuses with "Only root is allowed to use this option" and flushing
#     the cache needs root. If someone with root shows a cold cache diverging
#     within a pair, the verify shape becomes viable and this reasoning needs
#     revisiting.
#  2. THE RED ARM'S ATTRIBUTION IS SOUND ONLY BECAUSE OF A CHECKED FACT.
#     Running natively removes ALL of hermit, not just this mount. That isolates
#     this control only because /var/run/nscd is the ONLY /run or /var mount in
#     container.rs -- verified. Add another mount over that path and the arm
#     stops isolating this one.
#
# HOST-PORTABLE BY CONSTRUCTION: the mount is conditional on the host directory
# existing. On a host with no /var/run/nscd nothing is mounted, the guest sees no
# directory, and the assertion still holds -- it says "no host nscd leaks in",
# which is true either way.

set -euo pipefail

nscd_dir=/var/run/nscd

case ${1:-} in
    --prepare) exit 0 ;;
    --run)
        if [[ -S $nscd_dir/socket ]]; then
            printf 'nscd-neutralised FAIL: host nscd socket is visible at %s/socket\n' "$nscd_dir"
            exit 1
        fi
        # `find -mindepth 1` prints nothing both when the directory is the empty
        # mount and when it does not exist at all; both satisfy the contract.
        # Using find rather than `ls -A` also keeps shellcheck SC2012 quiet and
        # is correct for names that are not alphanumeric.
        leaked=$(find "$nscd_dir" -mindepth 1 -printf '%f ' 2>/dev/null || true)
        if [[ -n $leaked ]]; then
            printf 'nscd-neutralised FAIL: %s is not empty: %s\n' "$nscd_dir" "$leaked"
            exit 1
        fi
        printf 'nscd-neutralised=ok socket=absent dir=empty\n'
        ;;
    *) echo "usage: $0 --prepare|--run" >&2; exit 2 ;;
esac
