#!/usr/bin/env bash
# Copyright (c) Meta Platforms, Inc. and affiliates.
# All rights reserved.
#
# This source code is licensed under the BSD-style license found in the
# LICENSE file in the root directory of this source tree.
#
# Read one branch setting of ci/landing-branches.env.
#
#   source scripts/lib/landing_branch.sh
#   branch=$(landing_branch HERMIT_LANDING_BRANCH) || exit 2
#
# or, without sourcing, `bash scripts/lib/landing_branch.sh KEY [ROOT]`. The
# environment variable named KEY wins when it is set; otherwise KEY's line in
# ci/landing-branches.env of the checkout that holds this file (or of ROOT).
# The branch is printed on stdout. A setting that is missing, empty, repeated
# or not a plain branch name prints the reason on stderr and returns 1: there
# is no default branch to fall back to. ci/manifest-plan/src/landing_branch.rs
# and scripts/landing_branch.py implement the same rule.

landing_branch_root=$(cd -- "$(dirname -- "${BASH_SOURCE[0]}")/../.." && pwd)

# Whether NAME is a plain branch name: '/'-separated components, each starting
# with a letter or digit, made of letters, digits, '.', '_' and '-', and not
# ending in '.' or '.lock'; no '..' anywhere.
landing_branch_valid() { # NAME
    local component='[A-Za-z0-9]([A-Za-z0-9._-]*[A-Za-z0-9_-])?'
    [[ $1 =~ ^$component(/$component)*$ ]] || return 1
    [[ $1 != *..* && $1 != *.lock && $1 != *.lock/* ]]
}

landing_branch() { # KEY [ROOT]
    local key=$1 root=${2:-$landing_branch_root}
    local file="$root/ci/landing-branches.env" from value line number=0 found=0
    if [[ ! $key =~ ^[A-Z][A-Z0-9_]*$ ]]; then
        printf 'landing_branch: %q is not a setting name\n' "$key" >&2
        return 1
    fi
    if [[ -n ${!key+set} ]]; then
        from="environment variable $key"
        value=${!key}
    else
        from=$file
        if [[ ! -f $file || ! -r $file ]]; then
            printf 'landing_branch: cannot read %s, and %s is not set\n' "$file" "$key" >&2
            return 1
        fi
        while IFS= read -r line || [[ -n $line ]]; do
            number=$((number + 1))
            line=${line%$'\r'}
            [[ $line =~ ^[[:space:]]*(#.*)?$ ]] && continue
            if [[ ! $line =~ ^([A-Z][A-Z0-9_]*)=(.*)$ ]]; then
                printf 'landing_branch: %s:%d is not a KEY=value line\n' "$file" "$number" >&2
                return 1
            fi
            [[ ${BASH_REMATCH[1]} == "$key" ]] || continue
            if ((found)); then
                printf 'landing_branch: %s sets %s more than once\n' "$file" "$key" >&2
                return 1
            fi
            found=1
            value=${BASH_REMATCH[2]}
        done <"$file"
        if ((!found)); then
            printf 'landing_branch: %s does not set %s\n' "$file" "$key" >&2
            return 1
        fi
    fi
    if ! landing_branch_valid "$value"; then
        printf 'landing_branch: %s gives %s the value %q, which is not a branch name\n' \
            "$from" "$key" "$value" >&2
        return 1
    fi
    printf '%s\n' "$value"
}

if [[ ${BASH_SOURCE[0]} == "$0" ]]; then
    set -euo pipefail
    if (($# < 1 || $# > 2)); then
        echo 'usage: bash scripts/lib/landing_branch.sh KEY [ROOT]' >&2
        exit 2
    fi
    landing_branch "$@"
fi
