#!/usr/bin/env bash
# Copyright (c) Meta Platforms, Inc. and affiliates.
# All rights reserved.
#
# This source code is licensed under the BSD-style license found in the
# LICENSE file in the root directory of this source tree.
set -euo pipefail

# Surface: a REAL /proc consumer walking the process table.
#
# The corpus read individual /proc files (proc-uptime, proc-random-uuid) but
# nothing walked /proc as a directory and parsed per-process state. ps does
# exactly that: getdents on /proc, then stat/open/read of per-pid stat, status
# and cmdline. That combines the directory-enumeration surface with the
# pid/uid/comm virtualization surface in one real program.
#
# Fields are chosen to be relational, not host-absolute: pid/ppid under hermit
# are virtualized, state and comm are properties of the guest tree itself. No
# CPU-time or start-time column is requested -- those are virtual-clock
# observables already covered elsewhere, and including them would put a second
# moving part in this entry's oracle.
case ${1:-} in
    --prepare) exit 0 ;;
    --run)
        # Walk the whole table. Sorted so the report does not depend on the
        # order /proc happens to enumerate, which is a separate contract.
        work="${E2E_TMPDIR:-/tmp}"
        mkdir -p "$work"
        ps -eo pid,ppid,state,comm --no-headers | sort -n >"$work/table.txt"
        printf 'PS-TABLE\n'
        cat "$work/table.txt"
        rows=$(wc -l <"$work/table.txt" | tr -d '[:space:]')
        printf 'PS-ROWS %s\n' "$rows"

        # The guest's own view of itself, via /proc through a second real
        # program, must agree with what the shell already knows.
        self_comm=$(ps -o comm= -p $$)
        printf 'SELF-COMM %s\n' "$self_comm"
        # Compare ps's view of THIS shell's parent against the shell's own
        # $PPID. Reading /proc/self/stat here instead would be wrong: the awk
        # would be a child of the shell, so its ppid is the shell's pid, and the
        # check would report a mismatch on every correct system.
        if [ "$(ps -o ppid= -p $$ | tr -d '[:space:]')" = "$PPID" ]; then
            ppid_matches=yes
        else
            ppid_matches=no
        fi
        printf 'SELF-PPID-MATCHES %s\n' "$ppid_matches"

        # A directory walk of /proc that is not mediated by ps, to pin the
        # numeric-entry set the two views must share.
        proc_pids=0
        for proc_entry in /proc/[0-9]*; do
            if [ -d "$proc_entry" ]; then
                proc_pids=$((proc_pids + 1))
            fi
        done
        printf 'PROC-PIDS %s\n' "$proc_pids"

        if [ "$rows" -lt 2 ] || [ -z "$self_comm" ] || \
            [ "$ppid_matches" != yes ] || [ "$proc_pids" -lt 2 ]; then
            echo "process-table contract failed: rows=$rows self_comm=$self_comm ppid_matches=$ppid_matches proc_pids=$proc_pids" >&2
            exit 1
        fi
        ;;
    *) echo "usage: $0 --prepare|--run" >&2; exit 2 ;;
esac
