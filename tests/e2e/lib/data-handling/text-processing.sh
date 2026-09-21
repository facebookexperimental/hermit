#!/usr/bin/env bash
# Copyright (c) Meta Platforms, Inc. and affiliates.
# All rights reserved.
#
# This source code is licensed under the BSD-style license found in the
# LICENSE file in the root directory of this source tree.

set -euo pipefail

if [[ ${1:-} == --guest ]]; then
    selector=$(od -An -N1 -tu1 /dev/urandom)
    if ((selector % 2 == 0)); then
        locale=C
    else
        locale=en_US.utf8
    fi

    printf 'A\na\nz\n\303\244\nA\na\n' |
        LC_ALL="$locale" sort |
        LC_ALL="$locale" uniq -c |
        LC_ALL="$locale" awk -v locale="$locale" \
            'BEGIN { print "locale=" locale } { print $2 ":" $1 }'
    exit 0
fi

# shellcheck source=tests/e2e/lib/data-handling/common.bash
source "$(dirname -- "$0")/common.bash"
require_tools od sort uniq awk locale grep
if ! locale -a | grep -Fqx en_US.utf8; then
    echo 'required locale is unavailable: en_US.utf8' >&2
    exit 1
fi
export NATIVE_ATTEMPTS=16
assert_nondeterminism_removed locale-text-pipeline "$(readlink -f "$0")" --guest
