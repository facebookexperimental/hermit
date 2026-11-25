#!/usr/bin/env bash
# Copyright (c) Meta Platforms, Inc. and affiliates.
# All rights reserved.
#
# This source code is licensed under the BSD-style license found in the
# LICENSE file in the root directory of this source tree.

# Whether the checkout at ROOT carries the file RELATIVE: the one rule every
# export skip uses, so a check that reads a file the GitHub export does not
# carry skips it there by name, and fails everywhere else the file is missing.
#
# usage: ci/checkout-carries.sh ROOT RELATIVE
#
# Exit 1 (does not carry it) exactly when all four hold:
#   - RELATIVE is one of STRIPPED below, the paths the export has never carried;
#   - nothing at all is at ROOT/RELATIVE (no file, directory or symlink);
#   - ROOT is the top level of a git worktree, so git answers for the checkout
#     at ROOT and not for a repository that merely encloses it;
#   - the commit checked out there does not track RELATIVE
#     (`git ls-tree HEAD -- RELATIVE` lists nothing).
# Exit 0 (carries it) otherwise. That includes any path the export carries,
# whose absence is a deletion that must fail, and whenever git cannot answer
# for ROOT: the caller then reads the file and fails by name, so a broken or
# partial checkout never skips. When git cannot answer, one line on stderr says
# so, with git's exit status.
# Exit 2 on a usage error.
#
# The answer comes from the checkout alone, never from the environment: a fresh
# clone with nothing set gets the same answer as a CI unit. Repository-location
# variables a hook or `git rebase --exec` exports are dropped, because ROOT
# names the checkout; pathspecs are literal and lazy fetching is off.

set -u
# Each `cd` below enters the path it names, never one a caller's CDPATH finds.
unset CDPATH

# The paths the GitHub export has never carried: ShipIt strips them, and no
# commit in its history has held them. Only these may be skipped. shim/BUCK and
# the generated gVisor BUCK belong to the open-source Buck layer, whose home is
# decided by dh-8zi9o4; the liteinst-runtime profile is fb-only. When the export
# starts carrying one, delete it here, and its skip goes with it.
STRIPPED=(
    shim/BUCK
    third-party/gvisor-syscall-tests/BUCK
    fb-only/liteinst-runtime-profile.bzl
)

usage() {
    printf 'usage: ci/checkout-carries.sh ROOT RELATIVE\n' >&2
    exit 2
}

[[ $# == 2 && -n $1 && -n $2 ]] || usage
root=$1 relative=$2
case /$relative/ in
    //* | */../* | */./*) usage ;;
esac

stripped=0
for path in "${STRIPPED[@]}"; do
    [[ $relative == "$path" ]] && stripped=1
done
((stripped)) || exit 0
[[ -e $root/$relative || -L $root/$relative ]] && exit 0

unset GIT_DIR GIT_WORK_TREE GIT_INDEX_FILE GIT_COMMON_DIR GIT_OBJECT_DIRECTORY \
    GIT_ALTERNATE_OBJECT_DIRECTORIES GIT_NAMESPACE GIT_PREFIX
export GIT_LITERAL_PATHSPECS=1 GIT_NO_LAZY_FETCH=1

unanswered() {
    printf 'checkout-carries: git could not answer for %s (%s); %s counts as carried\n' \
        "$root" "$1" "$relative" >&2
    exit 0
}

top=$(git -C "$root" rev-parse --show-toplevel 2>/dev/null) || unanswered "git rev-parse exit $?"
here=$(cd -- "$root" 2>/dev/null && pwd -P) || unanswered "cannot enter it"
[[ $here == "$(cd -- "$top" 2>/dev/null && pwd -P)" ]] ||
    unanswered "it is not its worktree's top level, $top"
listed=$(git -C "$root" ls-tree --name-only HEAD -- "$relative" 2>/dev/null) ||
    unanswered "git ls-tree exit $?"
[[ -n $listed ]] && exit 0
exit 1
