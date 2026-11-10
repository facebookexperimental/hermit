#!/usr/bin/bash
# Copyright (c) Meta Platforms, Inc. and affiliates.
# All rights reserved.
#
# This source code is licensed under the BSD-style license found in the
# LICENSE file in the root directory of this source tree.
#
# The run's root image is this script, executed by its #! line (an
# interpreter path, not env, so no second exec renames the task). Linux names
# the task by the script's file name, kbasename(bprm->filename) in
# begin_new_exec, so this shell's own /proc/<pid>/stat, the Name: line of its
# /proc/<pid>/status and its /proc/<pid>/comm all say stat-root.sh. The shell
# reads all three itself (builtin redirections, no child process) and prints
# the name each shows; a file it cannot open prints "unreadable".
set -u

case ${1:-} in
    --prepare) exit 0 ;;
    --run)
        read -r line < "/proc/$$/stat"
        stat_name=${line#* (}
        stat_name=${stat_name%) *}
        status_name=unreadable
        while read -r label value; do
            if [ "$label" = "Name:" ]; then
                status_name=$value
                break
            fi
        done < "/proc/$$/status"
        comm_name=unreadable
        { read -r comm_name < "/proc/$$/comm"; } 2>/dev/null || comm_name=unreadable
        printf 'stat=%s status=%s comm=%s\n' "$stat_name" "$status_name" "$comm_name"
        ;;
    *) echo "usage: $0 --prepare|--run" >&2; exit 2 ;;
esac
